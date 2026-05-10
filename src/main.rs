// sysmon - Beautiful Rust TUI System Monitor
// Copyright (c) 2026 Isaac Henry
// SPDX-License-Identifier: MIT
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self,
        Event, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute, queue,
    style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType,
        EnterAlternateScreen, LeaveAlternateScreen, size,
    },
};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{stdout, BufWriter, Write};
use std::time::{Duration, Instant};

// ═══════════════════════════════════════════════════════════════════════════════
// DOUBLE BUFFER — only writes cells that actually changed
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, PartialEq)]
struct Cell {
    ch: char,
    fg: Color,
    bold: bool,
    dim: bool,
    rev: bool,
}

impl Default for Cell {
    fn default() -> Self { Cell { ch: ' ', fg: Color::Reset, bold: false, dim: false, rev: false } }
}

struct Buffer {
    w: u16, h: u16,
    cells: Vec<Cell>,
}

impl Buffer {
    fn new(w: u16, h: u16) -> Self {
        Buffer { w, h, cells: vec![Cell::default(); w as usize * h as usize] }
    }

    fn idx(&self, x: u16, y: u16) -> Option<usize> {
        if x >= self.w || y >= self.h { return None; }
        Some(y as usize * self.w as usize + x as usize)
    }

    fn set(&mut self, x: u16, y: u16, ch: char, fg: Color, bold: bool, dim: bool, rev: bool) {
        if let Some(i) = self.idx(x, y) {
            self.cells[i] = Cell { ch, fg, bold, dim, rev };
        }
    }

    fn set_str(&mut self, x: u16, y: u16, s: &str, fg: Color, bold: bool, dim: bool, rev: bool) {
        for (i, ch) in s.chars().enumerate() {
            self.set(x + i as u16, y, ch, fg, bold, dim, rev);
        }
    }

    // Flush only changed cells to the terminal
    fn flush_diff(&self, prev: &Buffer, out: &mut impl Write) {
        let mut cursor: Option<(u16, u16)> = None;
        let mut last_fg = Color::Reset;
        let mut last_bold = false; let mut last_dim = false; let mut last_rev = false;

        for y in 0..self.h {
            for x in 0..self.w {
                let i = y as usize * self.w as usize + x as usize;
                let curr = &self.cells[i];
                let prev_cell = &prev.cells[i];
                if curr == prev_cell { continue; }

                if cursor.map_or(true, |(cx, cy)| cy != y || cx + 1 != x) {
                    queue!(out, MoveTo(x, y)).unwrap();
                }
                cursor = Some((x, y));

                // Attributes
                if curr.bold != last_bold || curr.dim != last_dim || curr.rev != last_rev {
                    queue!(out, SetAttribute(Attribute::Reset)).unwrap();
                    last_bold = false; last_dim = false; last_rev = false; last_fg = Color::Reset;
                }
                if curr.bold && !last_bold {
                    queue!(out, SetAttribute(Attribute::Bold)).unwrap();
                    last_bold = true;
                }
                if curr.dim && !last_dim {
                    queue!(out, SetAttribute(Attribute::Dim)).unwrap();
                    last_dim = true;
                }
                if curr.rev && !last_rev {
                    queue!(out, SetAttribute(Attribute::Reverse)).unwrap();
                    last_rev = true;
                }

                // Color
                if curr.fg != last_fg {
                    if curr.fg == Color::Reset {
                        queue!(out, ResetColor).unwrap();
                    } else {
                        queue!(out, SetForegroundColor(curr.fg)).unwrap();
                    }
                    last_fg = curr.fg;
                }

                write!(out, "{}", curr.ch).unwrap();
            }
        }
        queue!(out, SetAttribute(Attribute::Reset), ResetColor).unwrap();
        out.flush().unwrap();
    }
}

// Canvas — draw into a Buffer with a simple API
struct Canvas<'a> {
    buf: &'a mut Buffer,
    fg: Color,
    bold: bool,
    dim: bool,
    rev: bool,
}

impl<'a> Canvas<'a> {
    fn new(buf: &'a mut Buffer) -> Self {
        Canvas { buf, fg: Color::White, bold: false, dim: false, rev: false }
    }

    fn fg(mut self, c: Color) -> Self { self.fg = c; self }
    fn bold(mut self) -> Self { self.bold = true; self }
    fn dim(mut self) -> Self { self.dim = true; self }
    fn rev(mut self) -> Self { self.rev = true; self }
    fn rev_if(self, cond: bool) -> Self { if cond { self.rev() } else { self } }

    fn print(&mut self, x: u16, y: u16, s: &str) {
        self.buf.set_str(x, y, s, self.fg, self.bold, self.dim, self.rev);
    }

    fn print_truncated(&mut self, x: u16, y: u16, s: &str, max_w: u16) {
        let t: String = s.chars().take(max_w as usize).collect();
        let padded = format!("{:<width$}", t, width = max_w as usize);
        self.buf.set_str(x, y, &padded, self.fg, self.bold, self.dim, self.rev);
    }

    fn fill_line(&mut self, x: u16, y: u16, w: u16, ch: char) {
        for i in 0..w {
            self.buf.set(x+i, y, ch, self.fg, self.bold, self.dim, self.rev);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SYSTEM READERS
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Default)]
struct CpuStat { user:u64,nice:u64,system:u64,idle:u64,iowait:u64,irq:u64,softirq:u64 }
impl CpuStat {
    fn total(&self) -> u64 { self.user+self.nice+self.system+self.idle+self.iowait+self.irq+self.softirq }
    fn idle_total(&self) -> u64 { self.idle+self.iowait }
}

fn parse_cpu_stats() -> Vec<CpuStat> {
    fs::read_to_string("/proc/stat").unwrap_or_default().lines()
        .filter(|l| l.starts_with("cpu"))
        .filter_map(|line| {
            let p: Vec<u64> = line.split_whitespace().skip(1)
                .filter_map(|s| s.parse().ok()).collect();
            if p.len() < 7 { return None; }
            Some(CpuStat{user:p[0],nice:p[1],system:p[2],idle:p[3],iowait:p[4],irq:p[5],softirq:p[6]})
        }).collect()
}

fn cpu_pct(prev: &CpuStat, curr: &CpuStat) -> f64 {
    let td = curr.total().saturating_sub(prev.total()) as f64;
    let id = curr.idle_total().saturating_sub(prev.idle_total()) as f64;
    if td == 0.0 { 0.0 } else { ((1.0-id/td)*100.0).clamp(0.0,100.0) }
}

struct MemInfo { total:u64,available:u64,swap_total:u64,swap_free:u64,buffers:u64,cached:u64 }

fn parse_meminfo() -> MemInfo {
    let mut m = MemInfo{total:1,available:0,swap_total:0,swap_free:0,buffers:0,cached:0};
    for line in fs::read_to_string("/proc/meminfo").unwrap_or_default().lines() {
        let mut p = line.split_whitespace();
        let k = p.next().unwrap_or("");
        let v: u64 = p.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        match k {
            "MemTotal:"     => m.total     = v,
            "MemAvailable:" => m.available = v,
            "SwapTotal:"    => m.swap_total= v,
            "SwapFree:"     => m.swap_free = v,
            "Buffers:"      => m.buffers   = v,
            "Cached:"       => m.cached    = v,
            _ => {}
        }
    }
    m
}

#[derive(Clone,Default)]
struct NetStat { rx:u64,tx:u64 }

fn parse_net() -> NetStat {
    let mut t = NetStat::default();
    for line in fs::read_to_string("/proc/net/dev").unwrap_or_default().lines().skip(2) {
        let mut p = line.split_whitespace();
        let iface = p.next().unwrap_or("").trim_end_matches(':');
        if iface == "lo" { continue; }
        let rx: u64 = p.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let tx: u64 = p.nth(7).and_then(|s| s.parse().ok()).unwrap_or(0);
        t.rx += rx; t.tx += tx;
    }
    t
}

#[derive(Default)]
struct DiskStat { r:u64,w:u64 }

fn parse_disk() -> DiskStat {
    let mut d = DiskStat::default();
    for line in fs::read_to_string("/proc/diskstats").unwrap_or_default().lines() {
        let p: Vec<&str> = line.split_whitespace().collect();
        if p.len() < 14 { continue; }
        let n = p[2];
        if n.starts_with("loop")||n.starts_with("dm-") { continue; }
        if (n.starts_with("sd")||n.starts_with("hd"))&&n.len()>3 { continue; }
        if n.starts_with("nvme")&&n.contains('p') { continue; }
        d.r += p[5].parse::<u64>().unwrap_or(0);
        d.w += p[9].parse::<u64>().unwrap_or(0);
    }
    d
}

fn read_temp() -> Option<f64> {
    if let Ok(dir) = fs::read_dir("/sys/class/hwmon") {
        for e in dir.filter_map(|e| e.ok()) {
            if let Ok(s) = fs::read_to_string(e.path().join("temp1_input")) {
                if let Ok(v) = s.trim().parse::<u64>() {
                    let c = v as f64/1000.0;
                    if c>1.0&&c<120.0 { return Some(c); }
                }
            }
        }
    }
    for i in 0..8 {
        if let Ok(s) = fs::read_to_string(format!("/sys/class/thermal/thermal_zone{}/temp",i)) {
            if let Ok(v) = s.trim().parse::<u64>() {
                let c = v as f64/1000.0;
                if c>1.0&&c<120.0 { return Some(c); }
            }
        }
    }
    None
}

fn read_freqs() -> Vec<u64> {
    (0..256).map_while(|i|
        fs::read_to_string(format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_cur_freq",i))
            .ok().and_then(|s| s.trim().parse::<u64>().ok()).map(|k| k/1000)
    ).collect()
}

fn read_uptime() -> u64 {
    fs::read_to_string("/proc/uptime").unwrap_or_default()
        .split_whitespace().next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) as u64
}

fn read_loadavg() -> (f64,f64,f64) {
    let s = fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let mut p = s.split_whitespace();
    (p.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
     p.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
     p.next().and_then(|v| v.parse().ok()).unwrap_or(0.0))
}

fn read_hostname() -> String {
    fs::read_to_string("/etc/hostname").unwrap_or_default().trim().to_string()
}

fn count_procs() -> usize {
    fs::read_dir("/proc").map(|d|
        d.filter_map(|e| e.ok())
         .filter(|e| e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()))
         .count()
    ).unwrap_or(0)
}

#[derive(Clone)]
struct Proc {
    pid:u32,ppid:u32,name:String,user:String,
    state:char,cpu:f64,mem_kb:u64,threads:u32,
    cmd:String,nice:i32,virt_kb:u64,
    children:Vec<u32>,
}

fn build_uid_cache() -> HashMap<u32,String> {
    let mut map = HashMap::new();
    for line in fs::read_to_string("/etc/passwd").unwrap_or_default().lines() {
        let p: Vec<&str> = line.split(':').collect();
        if p.len() >= 3 {
            if let Ok(uid) = p[2].parse::<u32>() { map.insert(uid, p[0].to_string()); }
        }
    }
    map
}

fn read_procs_all() -> Vec<Proc> {
    let uptime = read_uptime() as f64;
    let uid_cache = build_uid_cache();
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir("/proc") else { return out; };
    for entry in dir.filter_map(|e| e.ok()) {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue; };
        let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat",pid)) else { continue; };
        let ns = stat.find('(').unwrap_or(0)+1;
        let ne = stat.rfind(')').unwrap_or(ns);
        let name = stat[ns..ne].to_string();
        let rest: Vec<&str> = stat[ne+2..].split_whitespace().collect();
        if rest.len() < 22 { continue; }
        let state   = rest[0].chars().next().unwrap_or('?');
        let ppid: u32   = rest[1].parse().unwrap_or(0);
        let nice: i32   = rest[16].parse().unwrap_or(0);
        let utime:u64   = rest[11].parse().unwrap_or(0);
        let stime:u64   = rest[12].parse().unwrap_or(0);
        let start:u64   = rest[19].parse().unwrap_or(0);
        let threads:u32 = rest[17].parse().unwrap_or(0);
        let virt_kb:u64 = rest[20].parse::<u64>().unwrap_or(0)/1024;
        let elapsed = (uptime-(start/100) as f64).max(0.001);
        let cpu = ((utime+stime) as f64/100.0)/elapsed*100.0;
        let status = fs::read_to_string(format!("/proc/{}/status",pid)).unwrap_or_default();
        let mut mem_kb=0u64; let mut uid=0u32;
        for sl in status.lines() {
            if sl.starts_with("VmRSS:") { mem_kb=sl.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0); }
            if sl.starts_with("Uid:")   { uid   =sl.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0); }
        }
        let cmd: String = fs::read_to_string(format!("/proc/{}/cmdline",pid))
            .unwrap_or_default().replace('\0'," ").trim().chars().take(120).collect();
        let user = uid_cache.get(&uid).cloned().unwrap_or_else(|| uid.to_string());
        out.push(Proc{pid,ppid,name,user,state,cpu,mem_kb,threads,cmd,nice,virt_kb,children:Vec::new()});
    }
    out
}

#[derive(Default)]
struct GpuInfo { name:String,pct:f64,mem_used:u64,mem_total:u64,temp:Option<f64> }

fn read_gpu() -> Option<GpuInfo> {
    if let Ok(o) = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu",
               "--format=csv,noheader,nounits"]).output() {
        let s = String::from_utf8_lossy(&o.stdout);
        let p: Vec<&str> = s.trim().split(',').map(|v| v.trim()).collect();
        if p.len()>=5&&!p[0].is_empty() {
            return Some(GpuInfo{name:p[0].into(),pct:p[1].parse().unwrap_or(0.0),
                mem_used:p[2].parse().unwrap_or(0),mem_total:p[3].parse().unwrap_or(0),
                temp:p[4].parse().ok()});
        }
    }
    for card in 0..4 {
        let base = format!("/sys/class/drm/card{}/device",card);
        let Ok(busy) = fs::read_to_string(format!("{}/gpu_busy_percent",base)) else { continue; };
        let pct: f64 = busy.trim().parse().unwrap_or(0.0);
        let mem_used  = fs::read_to_string(format!("{}/mem_info_vram_used",base)).ok()
            .and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0)/1_048_576;
        let mem_total = fs::read_to_string(format!("{}/mem_info_vram_total",base)).ok()
            .and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0)/1_048_576;
        let name = fs::read_to_string(format!("{}/product_name",base))
            .ok().map(|s| s.trim().to_string())
            .unwrap_or_else(|| format!("AMD GPU (card{})",card));
        let temp = (|| -> Option<f64> {
            let hd = fs::read_dir(format!("{}/hwmon",base)).ok()?;
            for e in hd.filter_map(|e| e.ok()) {
                for ti in 1..=5 {
                    if let Ok(s) = fs::read_to_string(e.path().join(format!("temp{}_input",ti))) {
                        if let Ok(v) = s.trim().parse::<u64>() {
                            let c = v as f64/1000.0;
                            if c>1.0&&c<120.0 { return Some(c); }
                        }
                    }
                }
            }
            None
        })();
        return Some(GpuInfo{name,pct,mem_used,mem_total,temp});
    }
    None
}

// ═══════════════════════════════════════════════════════════════════════════════
// SORT / TREE
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone,Copy,PartialEq)]
enum SortCol { Cpu,Mem,Pid,Name,User,State,Threads,Virt }

impl SortCol {
    fn label(self) -> &'static str {
        match self { SortCol::Cpu=>"CPU",SortCol::Mem=>"MEM",SortCol::Pid=>"PID",
                     SortCol::Name=>"NAME",SortCol::User=>"USER",SortCol::State=>"ST",
                     SortCol::Threads=>"THR",SortCol::Virt=>"VIRT" }
    }
}

fn build_tree(procs: &mut Vec<Proc>) {
    let pid_idx: HashMap<u32,usize> = procs.iter().enumerate().map(|(i,p)| (p.pid,i)).collect();
    for p in procs.iter_mut() { p.children.clear(); }
    let ppids: Vec<(u32,u32)> = procs.iter().map(|p| (p.pid,p.ppid)).collect();
    for (pid,ppid) in ppids {
        if let Some(&pi) = pid_idx.get(&ppid) { procs[pi].children.push(pid); }
    }
}

fn flatten_tree(procs: &[Proc]) -> Vec<(usize,usize)> {
    let pid_idx: HashMap<u32,usize> = procs.iter().enumerate().map(|(i,p)| (p.pid,i)).collect();
    let all_pids: std::collections::HashSet<u32> = procs.iter().map(|p| p.pid).collect();
    let mut result = Vec::new();
    let mut visited = vec![false; procs.len()];

    fn visit(idx:usize,depth:usize,procs:&[Proc],pid_idx:&HashMap<u32,usize>,
             visited:&mut Vec<bool>,result:&mut Vec<(usize,usize)>) {
        if visited[idx] { return; }
        visited[idx] = true;
        result.push((idx,depth));
        let mut ch: Vec<usize> = procs[idx].children.iter()
            .filter_map(|&cp| pid_idx.get(&cp).copied()).collect();
        ch.sort_by_key(|&i| procs[i].pid);
        for ci in ch { visit(ci,depth+1,procs,pid_idx,visited,result); }
    }

    let mut roots: Vec<usize> = procs.iter().enumerate()
        .filter(|(_,p)| !all_pids.contains(&p.ppid)||p.ppid==p.pid)
        .map(|(i,_)| i).collect();
    roots.sort_by_key(|&i| procs[i].pid);
    for root in roots { visit(root,0,procs,&pid_idx,&mut visited,&mut result); }
    for i in 0..procs.len() { if !visited[i] { result.push((i,0)); } }
    result
}

// ═══════════════════════════════════════════════════════════════════════════════
// UI STATE
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(PartialEq)]
enum UiMode { Normal,Search,Kill,Signal,Help }

struct Ui {
    mode:UiMode, sort_col:SortCol, sort_asc:bool,
    tree_view:bool, search:String, selected:usize, scroll:usize,
    kill_sig:i32, status_msg:String, status_ts:Instant,
    show_graphs:bool, proc_table_y:u16, proc_table_h:u16,
}

impl Ui {
    fn new() -> Self {
        Ui { mode:UiMode::Normal,sort_col:SortCol::Cpu,sort_asc:false,
             tree_view:false,search:String::new(),selected:0,scroll:0,
             kill_sig:15,status_msg:String::new(),status_ts:Instant::now(),
             show_graphs:true,proc_table_y:0,proc_table_h:0 }
    }
    fn set_status(&mut self, msg: impl Into<String>) {
        self.status_msg = msg.into(); self.status_ts = Instant::now();
    }
    fn clamp_selected(&mut self, len: usize) {
        if len==0 { self.selected=0; return; }
        if self.selected>=len { self.selected=len-1; }
    }
    fn ensure_visible(&mut self, rows: usize) {
        if self.selected<self.scroll { self.scroll=self.selected; }
        if rows>0 && self.selected>=self.scroll+rows { self.scroll=self.selected-rows+1; }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// STATE
// ═══════════════════════════════════════════════════════════════════════════════

const HIST: usize = 200;
fn nhist() -> VecDeque<f64> { VecDeque::from(vec![0.0;HIST]) }
fn push_h(h:&mut VecDeque<f64>,v:f64) { h.push_back(v); if h.len()>HIST { h.pop_front(); } }

struct State {
    prev_cpu:Vec<CpuStat>,prev_net:NetStat,prev_disk:DiskStat,
    cores:Vec<f64>,avg_hist:VecDeque<f64>,
    net_rx_hist:VecDeque<f64>,net_tx_hist:VecDeque<f64>,
    disk_r_hist:VecDeque<f64>,disk_w_hist:VecDeque<f64>,
    gpu_hist:VecDeque<f64>,
    net_rx:f64,net_tx:f64,disk_r:f64,disk_w:f64,
    mem:MemInfo,temp:Option<f64>,freqs:Vec<u64>,
    hostname:String,load:(f64,f64,f64),uptime:u64,
    all_procs:Vec<Proc>,proc_ts:Instant,
    gpu:Option<GpuInfo>,proc_count:usize,
}

impl State {
    fn new() -> Self {
        let cpu = parse_cpu_stats();
        let n = cpu.len().saturating_sub(1).max(1);
        let mut s = State {
            prev_cpu:cpu,prev_net:parse_net(),prev_disk:parse_disk(),
            cores:vec![0.0;n],avg_hist:nhist(),
            net_rx_hist:nhist(),net_tx_hist:nhist(),
            disk_r_hist:nhist(),disk_w_hist:nhist(),gpu_hist:nhist(),
            net_rx:0.0,net_tx:0.0,disk_r:0.0,disk_w:0.0,
            mem:parse_meminfo(),temp:read_temp(),freqs:read_freqs(),
            hostname:read_hostname(),load:read_loadavg(),uptime:read_uptime(),
            all_procs:Vec::new(),proc_ts:Instant::now()-Duration::from_secs(10),
            gpu:read_gpu(),proc_count:0,
        };
        s.proc_count=count_procs();
        s
    }

    fn refresh(&mut self, dt:f64) {
        let curr = parse_cpu_stats();
        if curr.len()>1 {
            while self.cores.len()<curr.len()-1 { self.cores.push(0.0); }
            self.cores = curr.iter().skip(1).zip(self.prev_cpu.iter().skip(1))
                .map(|(c,p)| cpu_pct(p,c)).collect();
        }
        let avg = if self.cores.is_empty() { 0.0 }
                  else { self.cores.iter().sum::<f64>()/self.cores.len() as f64 };
        push_h(&mut self.avg_hist,avg);
        self.prev_cpu=curr;

        let cn=parse_net();
        self.net_rx=cn.rx.saturating_sub(self.prev_net.rx) as f64/dt;
        self.net_tx=cn.tx.saturating_sub(self.prev_net.tx) as f64/dt;
        push_h(&mut self.net_rx_hist,self.net_rx); push_h(&mut self.net_tx_hist,self.net_tx);
        self.prev_net=cn;

        let cd=parse_disk();
        self.disk_r=cd.r.saturating_sub(self.prev_disk.r) as f64*512.0/dt;
        self.disk_w=cd.w.saturating_sub(self.prev_disk.w) as f64*512.0/dt;
        push_h(&mut self.disk_r_hist,self.disk_r); push_h(&mut self.disk_w_hist,self.disk_w);
        self.prev_disk=cd;

        self.mem=parse_meminfo(); self.temp=read_temp();
        self.freqs=read_freqs(); self.load=read_loadavg();
        self.uptime=read_uptime(); self.gpu=read_gpu();
        if let Some(ref g)=self.gpu { push_h(&mut self.gpu_hist,g.pct); }

        if self.proc_ts.elapsed()>Duration::from_millis(1500) {
            self.all_procs=read_procs_all();
            self.proc_count=count_procs();
            self.proc_ts=Instant::now();
        }
    }

    fn display_list(&mut self, ui:&Ui) -> Vec<usize> {
        if ui.tree_view {
            build_tree(&mut self.all_procs);
            let flat = flatten_tree(&self.all_procs);
            if !ui.search.is_empty() {
                let q = ui.search.to_lowercase();
                flat.into_iter().filter(|(i,_)| {
                    let p=&self.all_procs[*i];
                    p.name.to_lowercase().contains(&q)||p.user.to_lowercase().contains(&q)||
                    p.pid.to_string().contains(&q)||p.cmd.to_lowercase().contains(&q)
                }).map(|(i,_)| i).collect()
            } else { flat.into_iter().map(|(i,_)| i).collect() }
        } else {
            let mut idx: Vec<usize> = (0..self.all_procs.len()).collect();
            if !ui.search.is_empty() {
                let q = ui.search.to_lowercase();
                idx.retain(|&i| {
                    let p=&self.all_procs[i];
                    p.name.to_lowercase().contains(&q)||p.user.to_lowercase().contains(&q)||
                    p.pid.to_string().contains(&q)||p.cmd.to_lowercase().contains(&q)
                });
            }
            let col=ui.sort_col; let asc=ui.sort_asc;
            idx.sort_by(|&a,&b| {
                let pa=&self.all_procs[a]; let pb=&self.all_procs[b];
                let ord = match col {
                    SortCol::Cpu     => pa.cpu.partial_cmp(&pb.cpu).unwrap_or(std::cmp::Ordering::Equal),
                    SortCol::Mem     => pa.mem_kb.cmp(&pb.mem_kb),
                    SortCol::Pid     => pa.pid.cmp(&pb.pid),
                    SortCol::Name    => pa.name.cmp(&pb.name),
                    SortCol::User    => pa.user.cmp(&pb.user),
                    SortCol::State   => pa.state.cmp(&pb.state),
                    SortCol::Threads => pa.threads.cmp(&pb.threads),
                    SortCol::Virt    => pa.virt_kb.cmp(&pb.virt_kb),
                };
                if asc { ord } else { ord.reverse() }
            });
            idx
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// FORMATTING
// ═══════════════════════════════════════════════════════════════════════════════

fn clr(p:f64) -> Color { if p<60.0 { Color::Green } else if p<80.0 { Color::Yellow } else { Color::Red } }
fn fmt_rate(b:f64) -> String {
    if b>=1_073_741_824.0 { format!("{:.1}GB/s",b/1_073_741_824.0) }
    else if b>=1_048_576.0 { format!("{:.1}MB/s",b/1_048_576.0) }
    else if b>=1024.0 { format!("{:.0}KB/s",b/1024.0) }
    else { format!("{:.0}B/s",b) }
}
fn fmt_mem(kb:u64) -> String {
    if kb>=1_048_576 { format!("{:.2}GB",kb as f64/1_048_576.0) }
    else if kb>=1024 { format!("{:.0}MB",kb as f64/1024.0) }
    else { format!("{}KB",kb) }
}
fn fmt_up(s:u64) -> String {
    let (d,h,m)=(s/86400,(s%86400)/3600,(s%3600)/60);
    if d>0 { format!("{}d {:02}:{:02}",d,h,m) } else { format!("{:02}:{:02}:{:02}",h,m,s%60) }
}
fn state_col(c:char) -> Color {
    match c { 'R'=>Color::Green,'S'=>Color::Cyan,'D'=>Color::Yellow,'Z'=>Color::Red,_=>Color::DarkGrey }
}
fn state_lbl(c:char) -> &'static str {
    match c { 'R'=>"RUN",'S'=>"SLP",'D'=>"DSK",'Z'=>"ZMB",'T'=>"STP",_=>"???" }
}

// ═══════════════════════════════════════════════════════════════════════════════
// DRAW INTO BUFFER
// ═══════════════════════════════════════════════════════════════════════════════

fn draw_box_buf(buf:&mut Buffer,x:u16,y:u16,w:u16,h:u16,title:&str,tc:Color) {
    if w<4||h<2 { return; }
    let mut c = Canvas::new(buf).dim();
    c.print(x,y,"┌");
    c.fill_line(x+1,y,w-2,'─');
    c.print(x+w-1,y,"┐");
    for row in 1..h-1 {
        c.print(x,y+row,"│");
        c.print(x+w-1,y+row,"│");
    }
    c.print(x,y+h-1,"└");
    c.fill_line(x+1,y+h-1,w-2,'─');
    c.print(x+w-1,y+h-1,"┘");
    if !title.is_empty()&&w>6 {
        let t = format!(" {} ",title);
        let t: String = t.chars().take(w.saturating_sub(4) as usize).collect();
        Canvas::new(buf).fg(tc).bold().print(x+2,y,&t);
    }
}

fn draw_hbar_buf(buf:&mut Buffer,x:u16,y:u16,w:u16,pct:f64,color:Color) {
    if w==0 { return; }
    let filled = ((pct.clamp(0.0,100.0)/100.0)*w as f64).round() as u16;
    for i in 0..w {
        if i<filled { Canvas::new(buf).fg(color).print(x+i,y,"█"); }
        else        { Canvas::new(buf).dim().print(x+i,y,"░"); }
    }
}

fn draw_graph_buf(buf:&mut Buffer,x:u16,y:u16,w:u16,h:u16,
                  hist:&VecDeque<f64>,color:Color,fixed_max:Option<f64>) {
    if w==0||h==0 { return; }
    let max = fixed_max.unwrap_or_else(|| {
        let mut s: Vec<f64> = hist.iter().cloned().collect();
        s.sort_by(|a,b| a.partial_cmp(b).unwrap());
        s.get(s.len()*9/10).cloned().unwrap_or(0.0).max(1.0)
    });
    let data: Vec<f64> = hist.iter().cloned().collect();
    let dlen = data.len();
    let blocks = ['▁','▂','▃','▄','▅','▆','▇','█'];
    for col in 0..w {
        let di = if dlen==0 { 0 } else { ((col as usize*dlen)/w as usize).min(dlen-1) };
        let val = (data[di]/max).clamp(0.0,1.0);
        for row in 0..h {
            let ct = (h-row) as f64/h as f64;
            let cb = (h-row-1) as f64/h as f64;
            if val>=ct {
                Canvas::new(buf).fg(color).print(x+col,y+row,"█");
            } else if val>cb {
                let frac=(val-cb)/(1.0/h as f64);
                let bi=((frac*blocks.len() as f64) as usize).min(blocks.len()-1);
                Canvas::new(buf).fg(color).print(x+col,y+row,&blocks[bi].to_string());
            } else if row==h-1 {
                Canvas::new(buf).dim().print(x+col,y+row,"▁");
            } else {
                Canvas::new(buf).dim().print(x+col,y+row," ");
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// RENDER INTO BUFFER
// ═══════════════════════════════════════════════════════════════════════════════

fn render_to_buf(buf:&mut Buffer,s:&State,ui:&mut Ui,display:&[usize]) {
    let tw=buf.w; let th=buf.h;
    if tw<40||th<15 { return; }

    let ncores=s.cores.len();
    let avg=if ncores==0 { 0.0 } else { s.cores.iter().sum::<f64>()/ncores as f64 };
    let avg_c=clr(avg);
    let has_gpu=s.gpu.is_some();

    // Layout
    let body=th.saturating_sub(2);
    let (top_h,mid_h) = if ui.show_graphs {
        let t=((body as f32*0.40) as u16).max(12).min(body.saturating_sub(14));
        let m=((body as f32*0.25) as u16).max(8).min(body.saturating_sub(t+5));
        (t,m)
    } else { (0,0) };
    let bot_h=body.saturating_sub(top_h+mid_h);
    let top_y:u16=1; let mid_y=top_y+top_h; let bot_y=mid_y+mid_h;

    let cpu_w=(tw as f32*0.60) as u16;
    let ram_w=tw-cpu_w; let ram_x=cpu_w;
    let (net_w,disk_w,gpu_w,gpu_x) = if has_gpu {
        let g=(tw/5).max(22).min(tw/3); let n=(tw-g)/2; (n,tw-n-g,g,n+(tw-n-g))
    } else { let n=tw/2; (n,tw-n,0,tw) };

    // Header
    let ts=s.temp.map(|t| format!(" {:.0}°C",t)).unwrap_or_default();
    let freq=if s.freqs.is_empty() { 0 } else { s.freqs.iter().sum::<u64>()/s.freqs.len() as u64 };
    let la=s.load;
    let tree_i=if ui.tree_view { " [TREE]" } else { "" };
    let hdr=format!(" ◈ SYSMON  {}{}  {} MHz  ↑ {}  load {:.2} {:.2} {:.2}  {} procs{}",
        s.hostname,ts,freq,fmt_up(s.uptime),la.0,la.1,la.2,s.proc_count,tree_i);
    Canvas::new(buf).fg(Color::Cyan).bold().print_truncated(0,0,&hdr,tw);

    if ui.show_graphs {
        // CPU box
        let t=s.temp.map(|t| format!(" {:.0}°C",t)).unwrap_or_default();
        draw_box_buf(buf,0,top_y,cpu_w,top_h,&format!("CPU  {:.1}%{}",avg,t),avg_c);
        let iw=cpu_w.saturating_sub(2); let ih=top_h.saturating_sub(2);
        let half=(ncores+1)/2; let cw=iw/2;
        let bar_wc=cw.saturating_sub(12).max(4);
        let mbr=ih.saturating_sub(5); let bar_rows=(half as u16).min(mbr);
        let graph_h=ih.saturating_sub(bar_rows+1).max(4);

        for (i,&usage) in s.cores.iter().enumerate() {
            let ci=(i/half) as u16; let ri=(i%half) as u16;
            if ri>=bar_rows { continue; }
            let bx=1+ci*cw; let by=top_y+1+ri;
            Canvas::new(buf).dim().print(bx,by,&format!("{:>2} ",i));
            draw_hbar_buf(buf,bx+3,by,bar_wc,usage,clr(usage));
            Canvas::new(buf).fg(clr(usage)).print(bx+3+bar_wc+1,by,&format!("{:5.1}%",usage));
        }

        let lbl_y=top_y+1+bar_rows;
        Canvas::new(buf).dim().print(2,lbl_y,"avg ");
        Canvas::new(buf).fg(avg_c).bold().print(6,lbl_y,&format!("{:.1}%",avg));
        let peak=s.avg_hist.iter().cloned().fold(0.0f64,f64::max);
        Canvas::new(buf).dim().print(14,lbl_y,&format!("peak {:.1}%  {}s history",peak,HIST));
        draw_graph_buf(buf,1,lbl_y+1,iw,graph_h,&s.avg_hist,avg_c,Some(100.0));

        // RAM box
        let used=s.mem.total.saturating_sub(s.mem.available);
        let mpct=used as f64/s.mem.total as f64*100.0; let mc=clr(mpct);
        let sused=s.mem.swap_total.saturating_sub(s.mem.swap_free);
        let spct=if s.mem.swap_total>0 { sused as f64/s.mem.swap_total as f64*100.0 } else { 0.0 };
        let sc=clr(spct);
        draw_box_buf(buf,ram_x,top_y,ram_w,top_h,&format!("RAM  {} / {}",fmt_mem(used),fmt_mem(s.mem.total)),mc);
        let rx=ram_x+2; let bw=ram_w.saturating_sub(16).max(4);
        Canvas::new(buf).dim().print(rx,top_y+2,"RAM  ");
        draw_hbar_buf(buf,rx+5,top_y+2,bw,mpct,mc);
        Canvas::new(buf).fg(mc).print(rx+5+bw+1,top_y+2,&format!("{:5.1}%",mpct));
        Canvas::new(buf).dim().print(rx,top_y+4,"SWAP ");
        if s.mem.swap_total>0 {
            draw_hbar_buf(buf,rx+5,top_y+4,bw,spct,sc);
            Canvas::new(buf).fg(sc).print(rx+5+bw+1,top_y+4,&format!("{:5.1}%",spct));
        } else { Canvas::new(buf).dim().print(rx+5,top_y+4,"─── none ───"); }
        let details=[("Total",fmt_mem(s.mem.total),Color::White),("Used",fmt_mem(used),mc),
                     ("Free",fmt_mem(s.mem.available),Color::Green),("Bufs",fmt_mem(s.mem.buffers),Color::DarkGrey),
                     ("Cache",fmt_mem(s.mem.cached),Color::DarkGrey),("SwpT",fmt_mem(s.mem.swap_total),Color::DarkGrey),
                     ("SwpU",fmt_mem(sused),sc)];
        let two_cols=ram_w>30; let half_d=(details.len()+1)/2;
        for (i,(lbl,val,col)) in details.iter().enumerate() {
            let (dx,dy)=if two_cols { (rx+(i/half_d) as u16*(ram_w/2),top_y+6+(i%half_d) as u16) }
                        else { (rx,top_y+6+i as u16) };
            if dy>=top_y+top_h-1 { break; }
            Canvas::new(buf).dim().print(dx,dy,&format!("{:<6}",lbl));
            Canvas::new(buf).fg(*col).print(dx+6,dy,val);
        }

        // NET box
        draw_box_buf(buf,0,mid_y,net_w,mid_h,&format!("NET  ▼ {}  ▲ {}",fmt_rate(s.net_rx),fmt_rate(s.net_tx)),Color::Cyan);
        let ngh=mid_h.saturating_sub(3); let nhw=net_w.saturating_sub(3)/2;
        Canvas::new(buf).fg(Color::Green).print(2,mid_y+1,&format!("▼ RX  {}",fmt_rate(s.net_rx)));
        Canvas::new(buf).fg(Color::Yellow).print(2+nhw+1,mid_y+1,&format!("▲ TX  {}",fmt_rate(s.net_tx)));
        draw_graph_buf(buf,1,mid_y+2,nhw,ngh,&s.net_rx_hist,Color::Green,None);
        draw_graph_buf(buf,2+nhw,mid_y+2,nhw.max(1),ngh,&s.net_tx_hist,Color::Yellow,None);

        // DISK box
        draw_box_buf(buf,net_w,mid_y,disk_w,mid_h,&format!("DISK  R {}  W {}",fmt_rate(s.disk_r),fmt_rate(s.disk_w)),Color::Yellow);
        let dgh=mid_h.saturating_sub(3); let dhw=disk_w.saturating_sub(3)/2; let dx2=net_w;
        Canvas::new(buf).fg(Color::Cyan).print(dx2+2,mid_y+1,&format!("● Read  {}",fmt_rate(s.disk_r)));
        Canvas::new(buf).fg(Color::Magenta).print(dx2+2+dhw+1,mid_y+1,&format!("● Write {}",fmt_rate(s.disk_w)));
        draw_graph_buf(buf,dx2+1,mid_y+2,dhw,dgh,&s.disk_r_hist,Color::Cyan,None);
        draw_graph_buf(buf,dx2+2+dhw,mid_y+2,dhw.max(1),dgh,&s.disk_w_hist,Color::Magenta,None);

        // GPU box
        if has_gpu { if let Some(ref g)=s.gpu {
            let gc=clr(g.pct);
            draw_box_buf(buf,gpu_x,mid_y,gpu_w,mid_h,&format!("GPU {:.0}%",g.pct),gc);
            let gx2=gpu_x+2; let gbw=gpu_w.saturating_sub(4).max(1);
            Canvas::new(buf).dim().print_truncated(gx2,mid_y+1,&g.name,gbw);
            draw_hbar_buf(buf,gx2,mid_y+2,gbw,g.pct,gc);
            Canvas::new(buf).fg(Color::Cyan).print(gx2,mid_y+3,&format!("{}/{}",fmt_mem(g.mem_used*1024),fmt_mem(g.mem_total*1024)));
            if let Some(t)=g.temp { Canvas::new(buf).fg(clr(t.min(100.0))).print(gx2,mid_y+4,&format!("{:.0}°C",t)); }
            let ggh=mid_h.saturating_sub(6).max(2);
            draw_graph_buf(buf,gpu_x+1,mid_y+mid_h-ggh-1,gpu_w.saturating_sub(2),ggh,&s.gpu_hist,gc,Some(100.0));
        }}
    }

    // Process table
    ui.proc_table_y=bot_y; ui.proc_table_h=bot_h;
    if bot_h>=4 {
        let si=if !ui.search.is_empty() { format!(" [/{}]",ui.search) } else { String::new() };
        let title=format!("PROCESSES{}  {}{}  {} shown",
            si,ui.sort_col.label(),if ui.sort_asc{"▲"}else{"▼"},display.len());
        draw_box_buf(buf,0,bot_y,tw,bot_h,&title,Color::Magenta);

        // Column headers
        struct ColDef { x:u16, w:u16, col:SortCol, label:&'static str }
        let cols=[ColDef{x:2,w:7,col:SortCol::Pid,label:"PID"},
                  ColDef{x:10,w:17,col:SortCol::Name,label:"NAME"},
                  ColDef{x:28,w:12,col:SortCol::User,label:"USER"},
                  ColDef{x:41,w:5,col:SortCol::State,label:"ST"},
                  ColDef{x:47,w:7,col:SortCol::Cpu,label:"CPU%"},
                  ColDef{x:55,w:10,col:SortCol::Mem,label:"MEM"},
                  ColDef{x:66,w:8,col:SortCol::Virt,label:"VIRT"},
                  ColDef{x:75,w:5,col:SortCol::Threads,label:"THR"}];
        for cd in &cols {
            let arrow=if ui.sort_col==cd.col { if ui.sort_asc {"▲"} else {"▼"} } else {" "};
            let label=format!("{}{:<w$}",arrow,cd.label,w=cd.w as usize-1);
            if ui.sort_col==cd.col { Canvas::new(buf).fg(Color::White).bold().print(cd.x,bot_y+1,&label); }
            else                   { Canvas::new(buf).dim().print(cd.x,bot_y+1,&label); }
        }
        Canvas::new(buf).dim().print(81,bot_y+1,"COMMAND");

        // Separator line
        for i in 1..tw-1 { Canvas::new(buf).dim().print(i,bot_y+2,"─"); }

        let vis=bot_h.saturating_sub(4) as usize;
        let cmd_w=(tw as usize).saturating_sub(83).max(0);

        for (row_i,&pi) in display.iter().enumerate().skip(ui.scroll).take(vis) {
            let p=&s.all_procs[pi];
            let sy=bot_y+3+(row_i-ui.scroll) as u16;
            let sel=row_i==ui.selected;

            // Clear the full row first
            Canvas::new(buf).fg(Color::Reset).fill_line(1,sy,tw-2,' ');

            let base_fg = if sel { Color::White } else { Color::Reset };

            Canvas::new(buf).dim().rev_if(sel).print(2,sy,&format!("{:<7}",p.pid));
            Canvas::new(buf).fg(Color::White).bold().rev_if(sel)
                .print(10,sy,&format!("{:<17}",p.name.chars().take(16).collect::<String>()));
            Canvas::new(buf).fg(if sel{Color::White}else{Color::Cyan}).rev_if(sel)
                .print(28,sy,&format!("{:<12}",p.user.chars().take(11).collect::<String>()));
            Canvas::new(buf).fg(if sel{Color::White}else{state_col(p.state)}).rev_if(sel)
                .print(41,sy,&format!("{:<5}",state_lbl(p.state)));
            Canvas::new(buf).fg(if sel{Color::White}else{clr(p.cpu)}).rev_if(sel)
                .print(47,sy,&format!("{:>6.1}%",p.cpu));
            Canvas::new(buf).fg(base_fg).rev_if(sel)
                .print(55,sy,&format!("{:>9}",fmt_mem(p.mem_kb)));
            Canvas::new(buf).dim().rev_if(sel)
                .print(66,sy,&format!("{:>7}",fmt_mem(p.virt_kb)));
            Canvas::new(buf).dim().rev_if(sel)
                .print(75,sy,&format!("{:>4}",p.threads));
            Canvas::new(buf).dim().rev_if(sel)
                .print(81,sy,&p.cmd.chars().take(cmd_w).collect::<String>());
        }
    }

    // Footer
    let footer = match ui.mode {
        UiMode::Search => format!(" Search: {}_  [Enter] confirm  [Esc] cancel",ui.search),
        UiMode::Help   => " [/]search [k]kill [F9]signal [t]tree [g]graphs [r]reverse [c/m/p/n/u]sort [?]help [q]quit".into(),
        UiMode::Kill   => {
            let pid=display.get(ui.selected).map(|&i| s.all_procs[i].pid).unwrap_or(0);
            format!(" Kill PID {} with SIGTERM? [y]yes  [n]cancel  [F9]choose signal",pid)
        },
        UiMode::Signal => " Signal: [1]HUP [2]INT [3]QUIT [9]KILL [15]TERM — type number then Enter".into(),
        UiMode::Normal => {
            if !ui.status_msg.is_empty()&&ui.status_ts.elapsed().as_secs()<3 {
                format!(" ✓ {}",ui.status_msg)
            } else {
                " [/]search  [k]kill  [F9]sig  [t]tree  [g]graphs  [↑↓]scroll  [?]help  [q]quit".into()
            }
        }
    };
    let footer_col = match ui.mode {
        UiMode::Kill|UiMode::Signal => Color::Red,
        UiMode::Search => Color::Yellow,
        UiMode::Help   => Color::Cyan,
        UiMode::Normal if !ui.status_msg.is_empty()&&ui.status_ts.elapsed().as_secs()<3 => Color::Green,
        _ => Color::DarkGrey,
    };
    Canvas::new(buf).fg(footer_col).bold().print_truncated(0,th-1,&footer,tw);
}

// ═══════════════════════════════════════════════════════════════════════════════
// INPUT
// ═══════════════════════════════════════════════════════════════════════════════

fn send_signal(pid:u32,sig:i32) -> Result<(),String> {
    std::process::Command::new("kill").args([format!("-{}",sig),pid.to_string()]).output()
        .map_err(|e| e.to_string())
        .and_then(|o| if o.status.success() { Ok(()) }
                      else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) })
}

fn handle_input(ev:Event,s:&State,ui:&mut Ui,display:&[usize],vis:usize) -> bool {
    match ui.mode {
        UiMode::Help => {
            if let Event::Key(k)=ev { match k.code {
                KeyCode::Char('?')|KeyCode::Esc => ui.mode=UiMode::Normal, _ => {}
            }}
        }
        UiMode::Search => {
            if let Event::Key(k)=ev { match k.code {
                KeyCode::Esc       => { ui.search.clear(); ui.mode=UiMode::Normal; }
                KeyCode::Enter     => { ui.mode=UiMode::Normal; }
                KeyCode::Backspace => { ui.search.pop(); }
                KeyCode::Char(c)   => { ui.search.push(c); ui.selected=0; ui.scroll=0; }
                _ => {}
            }}
        }
        UiMode::Kill => {
            if let Event::Key(k)=ev { match k.code {
                KeyCode::Char('y')|KeyCode::Enter => {
                    if let Some(&i)=display.get(ui.selected) {
                        let pid=s.all_procs[i].pid;
                        match send_signal(pid,ui.kill_sig) {
                            Ok(_)  => ui.set_status(format!("Sent signal {} to PID {}",ui.kill_sig,pid)),
                            Err(e) => ui.set_status(format!("Error: {}",e)),
                        }
                    }
                    ui.mode=UiMode::Normal;
                }
                KeyCode::F(9)                  => ui.mode=UiMode::Signal,
                KeyCode::Char('n')|KeyCode::Esc=> ui.mode=UiMode::Normal,
                _ => {}
            }}
        }
        UiMode::Signal => {
            if let Event::Key(k)=ev { match k.code {
                KeyCode::Esc   => ui.mode=UiMode::Normal,
                KeyCode::Enter => {
                    if let Some(&i)=display.get(ui.selected) {
                        let pid=s.all_procs[i].pid;
                        match send_signal(pid,ui.kill_sig) {
                            Ok(_)  => ui.set_status(format!("Sent signal {} to PID {}",ui.kill_sig,pid)),
                            Err(e) => ui.set_status(format!("Error: {}",e)),
                        }
                    }
                    ui.mode=UiMode::Normal;
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    let d=c as i32-'0' as i32;
                    ui.kill_sig=if ui.kill_sig<10 { ui.kill_sig*10+d } else { d };
                }
                _ => {}
            }}
        }
        UiMode::Normal => {
            match ev {
                Event::Key(k) => {
                    if k.modifiers.contains(KeyModifiers::CONTROL)&&k.code==KeyCode::Char('c') { return true; }
                    match k.code {
                        KeyCode::Char('q')|KeyCode::Esc => return true,
                        KeyCode::Char('?') => ui.mode=UiMode::Help,
                        KeyCode::Char('/') => { ui.mode=UiMode::Search; ui.search.clear(); }
                        KeyCode::Char('k') => { if !display.is_empty() { ui.kill_sig=15; ui.mode=UiMode::Kill; } }
                        KeyCode::F(9)      => { if !display.is_empty() { ui.kill_sig=15; ui.mode=UiMode::Signal; } }
                        KeyCode::Char('t') => { ui.tree_view=!ui.tree_view; ui.selected=0; ui.scroll=0; }
                        KeyCode::Char('g') => ui.show_graphs=!ui.show_graphs,
                        KeyCode::Char('r') => ui.sort_asc=!ui.sort_asc,
                        KeyCode::Char('c') => { ui.sort_col=SortCol::Cpu;    ui.sort_asc=false; ui.tree_view=false; }
                        KeyCode::Char('m') => { ui.sort_col=SortCol::Mem;    ui.sort_asc=false; ui.tree_view=false; }
                        KeyCode::Char('p') => { ui.sort_col=SortCol::Pid;    ui.sort_asc=true;  ui.tree_view=false; }
                        KeyCode::Char('n') => { ui.sort_col=SortCol::Name;   ui.sort_asc=true;  ui.tree_view=false; }
                        KeyCode::Char('u') => { ui.sort_col=SortCol::User;   ui.sort_asc=true;  ui.tree_view=false; }
                        KeyCode::Char('v') => { ui.sort_col=SortCol::Virt;   ui.sort_asc=false; ui.tree_view=false; }
                        KeyCode::Up        => { if ui.selected>0 { ui.selected-=1; } }
                        KeyCode::Down      => { if ui.selected+1<display.len() { ui.selected+=1; } }
                        KeyCode::PageUp    => { ui.selected=ui.selected.saturating_sub(vis); }
                        KeyCode::PageDown  => { ui.selected=(ui.selected+vis).min(display.len().saturating_sub(1)); }
                        KeyCode::Home      => { ui.selected=0; ui.scroll=0; }
                        KeyCode::End       => { ui.selected=display.len().saturating_sub(1); }
                        _ => {}
                    }
                }
                Event::Mouse(MouseEvent{kind,row,column,..}) => {
                    match kind {
                        MouseEventKind::ScrollUp   => { if ui.selected>0 { ui.selected-=1; } }
                        MouseEventKind::ScrollDown => { if ui.selected+1<display.len() { ui.selected+=1; } }
                        MouseEventKind::Down(MouseButton::Left) => {
                            let ts=ui.proc_table_y+3;
                            let te=ui.proc_table_y+ui.proc_table_h-1;
                            if row>=ts&&row<te {
                                let clicked=ui.scroll+(row-ts) as usize;
                                if clicked<display.len() {
                                    if clicked==ui.selected { ui.kill_sig=15; ui.mode=UiMode::Kill; }
                                    else { ui.selected=clicked; }
                                }
                            }
                            // Column header click to sort
                            if row==ui.proc_table_y+1 {
                                let hdrs=[(2u16,9u16,SortCol::Pid),(10,27,SortCol::Name),
                                          (28,40,SortCol::User),(41,46,SortCol::State),
                                          (47,54,SortCol::Cpu),(55,65,SortCol::Mem),
                                          (66,74,SortCol::Virt),(75,80,SortCol::Threads)];
                                for (x1,x2,col) in hdrs {
                                    if column>=x1&&column<x2 {
                                        if ui.sort_col==col { ui.sort_asc=!ui.sort_asc; }
                                        else { ui.sort_col=col; ui.sort_asc=false; }
                                        ui.tree_view=false; break;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Event::Resize(_,_) => {
                    // Buffer will be recreated next frame
                }
                _ => {}
            }
        }
    }
    false
}

// ═══════════════════════════════════════════════════════════════════════════════
// MAIN
// ═══════════════════════════════════════════════════════════════════════════════

fn main() -> std::io::Result<()> {
    let mut out = BufWriter::with_capacity(1 << 20, stdout());
    enable_raw_mode()?;
    execute!(out, EnterAlternateScreen, Hide, Clear(ClearType::All))?;
    let result = run(&mut out);
    execute!(out, Show, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    result
}

fn compute_vis(th: u16, show_graphs: bool) -> usize {
    let body = th.saturating_sub(2);
    let top_h = if show_graphs { ((body as f32*0.40) as u16).max(12).min(body.saturating_sub(14)) } else { 0 };
    let mid_h = if show_graphs { ((body as f32*0.25) as u16).max(8).min(body.saturating_sub(top_h+5)) } else { 0 };
    body.saturating_sub(top_h+mid_h).saturating_sub(4) as usize
}

fn run(out: &mut impl Write) -> std::io::Result<()> {
    let mut state = State::new();
    state.all_procs = read_procs_all();
    let mut ui = Ui::new();
    let mut last = Instant::now();

    let (mut tw, mut th) = size()?;
    let mut prev_buf = Buffer::new(tw, th);
    // Fill prev with impossible values to force full repaint on first frame
    for c in prev_buf.cells.iter_mut() { c.ch = '\0'; }

    loop {
        // Rebuild buffer at current terminal size
        let (ntw, nth) = size().unwrap_or((tw, th));
        if ntw != tw || nth != th {
            tw = ntw; th = nth;
            // Force full repaint on resize — queue with the frame so no blank flash
            queue!(out, Clear(ClearType::All)).unwrap();
            prev_buf = Buffer::new(tw, th);
            for c in prev_buf.cells.iter_mut() { c.ch = '\0'; }
        }

        let mut cur_buf = Buffer::new(tw, th);
        let display = state.display_list(&ui);
        let vis = compute_vis(th, ui.show_graphs);

        ui.clamp_selected(display.len());
        ui.ensure_visible(vis);

        render_to_buf(&mut cur_buf, &state, &mut ui, &display);
        cur_buf.flush_diff(&prev_buf, out);
        prev_buf = cur_buf;

        // Event loop — drain events for up to 1s
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let now = Instant::now();
            if now >= deadline { break; }
            let wait = (deadline - now).min(Duration::from_millis(50));
            if event::poll(wait)? {
                let ev = event::read()?;
                // Skip redraws for mouse events that never change state
                let state_changing = !matches!(ev,
                    Event::Mouse(MouseEvent { kind: MouseEventKind::Moved
                        | MouseEventKind::Up(_) | MouseEventKind::Drag(_), .. }));
                let vis_pre = compute_vis(th, ui.show_graphs);
                if handle_input(ev, &state, &mut ui, &display, vis_pre) { return Ok(()); }
                if !state_changing { continue; }
                // Redraw immediately after input — no clear, just diff
                let vis_post = compute_vis(th, ui.show_graphs);
                let display2 = state.display_list(&ui);
                ui.clamp_selected(display2.len());
                ui.ensure_visible(vis_post);
                let mut cur2 = Buffer::new(tw, th);
                render_to_buf(&mut cur2, &state, &mut ui, &display2);
                cur2.flush_diff(&prev_buf, out);
                prev_buf = cur2;
            }
        }

        // Refresh system data
        let dt = last.elapsed().as_secs_f64();
        state.refresh(dt);
        last = Instant::now();
    }
}

