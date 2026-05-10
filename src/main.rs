// sysmon - Beautiful Rust TUI System Monitor
// Copyright (c) 2026 Isaac Henry
// SPDX-License-Identifier: MIT

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture,
        Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Sparkline, Table, TableState},
    Terminal,
};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::stdout;
use std::time::{Duration, Instant};

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
                    if c > 1.0 && c < 120.0 { return Some(c); }
                }
            }
        }
    }
    for i in 0..8 {
        if let Ok(s) = fs::read_to_string(format!("/sys/class/thermal/thermal_zone{}/temp",i)) {
            if let Ok(v) = s.trim().parse::<u64>() {
                let c = v as f64/1000.0;
                if c > 1.0 && c < 120.0 { return Some(c); }
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
    pid:u32, ppid:u32, name:String, user:String,
    state:char, cpu:f64, mem_kb:u64, virt_kb:u64,
    threads:u32, cmd:String, nice:i32,
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
        let utime: u64  = rest[11].parse().unwrap_or(0);
        let stime: u64  = rest[12].parse().unwrap_or(0);
        let start: u64  = rest[19].parse().unwrap_or(0);
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
            .unwrap_or_default().replace('\0'," ").trim().chars().take(100).collect();
        let user = uid_cache.get(&uid).cloned().unwrap_or_else(|| uid.to_string());
        out.push(Proc{pid,ppid,name,user,state,cpu,mem_kb,virt_kb,threads,cmd,nice});
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
// APP STATE
// ═══════════════════════════════════════════════════════════════════════════════

const HIST: usize = 120;
fn nhist() -> VecDeque<u64> { VecDeque::from(vec![0u64; HIST]) }
fn push_h(h: &mut VecDeque<u64>, v: f64) {
    h.push_back(v.round() as u64);
    if h.len() > HIST { h.pop_front(); }
}

#[derive(Clone,Copy,PartialEq)]
enum SortCol { Cpu,Mem,Pid,Name,User,Virt,Threads }

#[derive(PartialEq)]
enum UiMode { Normal,Search,Kill,Signal }

struct App {
    // system data
    prev_cpu: Vec<CpuStat>, prev_net: NetStat, prev_disk: DiskStat,
    cores: Vec<f64>, avg_hist: VecDeque<u64>,
    net_rx_hist: VecDeque<u64>, net_tx_hist: VecDeque<u64>,
    disk_r_hist: VecDeque<u64>, disk_w_hist: VecDeque<u64>,
    gpu_hist: VecDeque<u64>,
    net_rx: f64, net_tx: f64, disk_r: f64, disk_w: f64,
    mem: MemInfo, temp: Option<f64>, freqs: Vec<u64>,
    hostname: String, load: (f64,f64,f64), uptime: u64,
    all_procs: Vec<Proc>, proc_ts: Instant,
    gpu: Option<GpuInfo>, proc_count: usize,
    // ui state
    mode: UiMode,
    sort_col: SortCol, sort_asc: bool,
    search: String,
    table_state: TableState,
    scroll: usize,
    kill_sig: i32,
    status: String, status_ts: Instant,
    show_graphs: bool,
}

impl App {
    fn new() -> Self {
        let cpu = parse_cpu_stats();
        let n = cpu.len().saturating_sub(1).max(1);
        let mut a = App {
            prev_cpu: cpu, prev_net: parse_net(), prev_disk: parse_disk(),
            cores: vec![0.0;n], avg_hist: nhist(),
            net_rx_hist: nhist(), net_tx_hist: nhist(),
            disk_r_hist: nhist(), disk_w_hist: nhist(), gpu_hist: nhist(),
            net_rx:0.0, net_tx:0.0, disk_r:0.0, disk_w:0.0,
            mem: parse_meminfo(), temp: read_temp(), freqs: read_freqs(),
            hostname: read_hostname(), load: read_loadavg(), uptime: read_uptime(),
            all_procs: Vec::new(), proc_ts: Instant::now()-Duration::from_secs(10),
            gpu: read_gpu(), proc_count: 0,
            mode: UiMode::Normal,
            sort_col: SortCol::Cpu, sort_asc: false,
            search: String::new(),
            table_state: TableState::default(),
            scroll: 0,
            kill_sig: 15,
            status: String::new(), status_ts: Instant::now(),
            show_graphs: true,
        };
        a.proc_count = count_procs();
        a.table_state.select(Some(0));
        a
    }

    fn refresh(&mut self, dt: f64) {
        let curr = parse_cpu_stats();
        if curr.len() > 1 {
            while self.cores.len() < curr.len()-1 { self.cores.push(0.0); }
            self.cores = curr.iter().skip(1).zip(self.prev_cpu.iter().skip(1))
                .map(|(c,p)| cpu_pct(p,c)).collect();
        }
        let avg = if self.cores.is_empty() { 0.0 }
                  else { self.cores.iter().sum::<f64>()/self.cores.len() as f64 };
        push_h(&mut self.avg_hist, avg);
        self.prev_cpu = curr;

        let cn = parse_net();
        self.net_rx = cn.rx.saturating_sub(self.prev_net.rx) as f64/dt;
        self.net_tx = cn.tx.saturating_sub(self.prev_net.tx) as f64/dt;
        push_h(&mut self.net_rx_hist, self.net_rx/1024.0);
        push_h(&mut self.net_tx_hist, self.net_tx/1024.0);
        self.prev_net = cn;

        let cd = parse_disk();
        self.disk_r = cd.r.saturating_sub(self.prev_disk.r) as f64*512.0/dt;
        self.disk_w = cd.w.saturating_sub(self.prev_disk.w) as f64*512.0/dt;
        push_h(&mut self.disk_r_hist, self.disk_r/1024.0);
        push_h(&mut self.disk_w_hist, self.disk_w/1024.0);
        self.prev_disk = cd;

        self.mem = parse_meminfo(); self.temp = read_temp();
        self.freqs = read_freqs(); self.load = read_loadavg();
        self.uptime = read_uptime(); self.gpu = read_gpu();
        if let Some(ref g) = self.gpu { push_h(&mut self.gpu_hist, g.pct); }

        if self.proc_ts.elapsed() > Duration::from_millis(1500) {
            self.all_procs = read_procs_all();
            self.proc_count = count_procs();
            self.proc_ts = Instant::now();
        }
    }

    fn display_procs(&self) -> Vec<&Proc> {
        let mut v: Vec<&Proc> = self.all_procs.iter()
            .filter(|p| {
                if self.search.is_empty() { return true; }
                let q = self.search.to_lowercase();
                p.name.to_lowercase().contains(&q) ||
                p.user.to_lowercase().contains(&q) ||
                p.pid.to_string().contains(&q) ||
                p.cmd.to_lowercase().contains(&q)
            }).collect();
        let asc = self.sort_asc;
        v.sort_by(|a,b| {
            let ord = match self.sort_col {
                SortCol::Cpu     => a.cpu.partial_cmp(&b.cpu).unwrap_or(std::cmp::Ordering::Equal),
                SortCol::Mem     => a.mem_kb.cmp(&b.mem_kb),
                SortCol::Pid     => a.pid.cmp(&b.pid),
                SortCol::Name    => a.name.cmp(&b.name),
                SortCol::User    => a.user.cmp(&b.user),
                SortCol::Virt    => a.virt_kb.cmp(&b.virt_kb),
                SortCol::Threads => a.threads.cmp(&b.threads),
            };
            if asc { ord } else { ord.reverse() }
        });
        v
    }

    fn selected_idx(&self) -> usize {
        self.table_state.selected().unwrap_or(0)
    }

    fn move_up(&mut self, n: usize) {
        let i = self.selected_idx().saturating_sub(n);
        self.table_state.select(Some(i));
    }

    fn move_down(&mut self, n: usize, max: usize) {
        let i = (self.selected_idx()+n).min(max.saturating_sub(1));
        self.table_state.select(Some(i));
    }

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = s.into(); self.status_ts = Instant::now();
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// HELPERS
// ═══════════════════════════════════════════════════════════════════════════════

fn pct_color(p: f64) -> Color {
    if p < 60.0 { Color::Green } else if p < 80.0 { Color::Yellow } else { Color::Red }
}

fn fmt_rate(b: f64) -> String {
    if b >= 1_073_741_824.0 { format!("{:.1}GB/s",b/1_073_741_824.0) }
    else if b >= 1_048_576.0 { format!("{:.1}MB/s",b/1_048_576.0) }
    else if b >= 1024.0 { format!("{:.0}KB/s",b/1024.0) }
    else { format!("{:.0}B/s",b) }
}

fn fmt_mem(kb: u64) -> String {
    if kb >= 1_048_576 { format!("{:.2}GB",kb as f64/1_048_576.0) }
    else if kb >= 1024 { format!("{:.0}MB",kb as f64/1024.0) }
    else { format!("{}KB",kb) }
}

fn fmt_up(s: u64) -> String {
    let (d,h,m) = (s/86400,(s%86400)/3600,(s%3600)/60);
    if d>0 { format!("{}d {:02}:{:02}",d,h,m) } else { format!("{:02}:{:02}:{:02}",h,m,s%60) }
}

fn state_color(c: char) -> Color {
    match c { 'R'=>Color::Green,'S'=>Color::Cyan,'D'=>Color::Yellow,'Z'=>Color::Red,_=>Color::DarkGray }
}

fn state_label(c: char) -> &'static str {
    match c { 'R'=>"RUN",'S'=>"SLP",'D'=>"DSK",'Z'=>"ZMB",'T'=>"STP",_=>"???" }
}

// ═══════════════════════════════════════════════════════════════════════════════
// UI DRAW
// ═══════════════════════════════════════════════════════════════════════════════

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let size = f.size();

    // Overall vertical split: header / body / footer
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(size);

    // ── Header ────────────────────────────────────────────────────────────
    let la = app.load;
    let ts = app.temp.map(|t| format!(" {:.0}°C",t)).unwrap_or_default();
    let freq = if app.freqs.is_empty() { 0 } else { app.freqs.iter().sum::<u64>()/app.freqs.len() as u64 };
    let hdr_text = format!(" ◈ SYSMON  {}{}  {} MHz  ↑ {}  load {:.2} {:.2} {:.2}  {} procs",
        app.hostname, ts, freq, fmt_up(app.uptime), la.0,la.1,la.2, app.proc_count);
    f.render_widget(
        Paragraph::new(hdr_text).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        root[0]);

    // ── Body ──────────────────────────────────────────────────────────────
    let body = root[1];

    let (graph_area, proc_area) = if app.show_graphs {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Min(0)])
            .split(body);
        (Some(split[0]), split[1])
    } else {
        (None, body)
    };

    if let Some(graphs) = graph_area {
        // Top row: CPU | RAM
        let top_row = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(58), Constraint::Min(0)])
            .split(Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(60), Constraint::Min(0)])
                .split(graphs)[0]);

        draw_cpu(f, app, top_row[0]);
        draw_ram(f, app, top_row[1]);

        // Bottom row: NET | DISK | GPU
        let mid_area = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(60), Constraint::Min(0)])
            .split(graphs)[1];

        let has_gpu = app.gpu.is_some();
        let mid_row = if has_gpu {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(35), Constraint::Percentage(45), Constraint::Min(0)])
                .split(mid_area)
        } else {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Min(0)])
                .split(mid_area)
        };

        draw_net(f, app, mid_row[0]);
        draw_disk(f, app, mid_row[1]);
        if has_gpu && mid_row.len() > 2 { draw_gpu(f, app, mid_row[2]); }
    }

    draw_procs(f, app, proc_area);

    // ── Footer ────────────────────────────────────────────────────────────
    let footer_text = match app.mode {
        UiMode::Search => format!(" Search: {}_  [Enter] confirm  [Esc] cancel", app.search),
        UiMode::Kill   => {
            let procs = app.display_procs();
            let pid = procs.get(app.selected_idx()).map(|p| p.pid).unwrap_or(0);
            format!(" Kill PID {} (SIGTERM)? [y] yes  [n] cancel  [F9] choose signal", pid)
        }
        UiMode::Signal => format!(" Signal {}: [1]HUP [2]INT [9]KILL [15]TERM — type number then Enter", app.kill_sig),
        UiMode::Normal => {
            if !app.status.is_empty() && app.status_ts.elapsed().as_secs() < 3 {
                format!(" ✓ {}", app.status)
            } else {
                app.status.clear();
                " [/]search  [k]kill  [F9]signal  [g]graphs  [r]reverse  [c/m/p/n/u]sort  [?]help  [q]quit".into()
            }
        }
    };
    let footer_style = match app.mode {
        UiMode::Kill | UiMode::Signal => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        UiMode::Search => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        UiMode::Normal if !app.status.is_empty() && app.status_ts.elapsed().as_secs() < 3
            => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(Color::DarkGray),
    };
    f.render_widget(Paragraph::new(footer_text).style(footer_style), root[2]);
}

fn draw_cpu(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let ncores = app.cores.len();
    let avg = if ncores==0 { 0.0 } else { app.cores.iter().sum::<f64>()/ncores as f64 };
    let ts = app.temp.map(|t| format!(" {:.0}°C",t)).unwrap_or_default();
    let freq = if app.freqs.is_empty() { 0 } else { app.freqs.iter().sum::<u64>()/app.freqs.len() as u64 };
    let title = format!(" CPU  {:.1}%{}  {} MHz  {} cores ", avg, ts, freq, ncores);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(title, Style::default().fg(pct_color(avg)).add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 3 { return; }

    // Split inner: cores on top, sparkline on bottom
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(inner);

    // Per-core bars in two columns
    let half = (ncores+1)/2;
    let col_areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(split[0]);

    for col in 0..2 {
        let mut lines: Vec<Line> = Vec::new();
        for row in 0..half {
            let i = col*half+row;
            if i >= ncores { lines.push(Line::raw("")); continue; }
            let usage = app.cores[i];
            let c = pct_color(usage);
            let bar_w = (col_areas[col].width as usize).saturating_sub(12).max(1);
            let filled = ((usage/100.0)*bar_w as f64).round() as usize;
            let bar: String = "█".repeat(filled) + &"░".repeat(bar_w-filled);
            lines.push(Line::from(vec![
                Span::styled(format!("{:>2} ", i), Style::default().fg(Color::DarkGray)),
                Span::styled(bar, Style::default().fg(c)),
                Span::styled(format!(" {:5.1}%", usage), Style::default().fg(c)),
            ]));
        }
        f.render_widget(Paragraph::new(lines), col_areas[col]);
    }

    // CPU sparkline
    let data: Vec<u64> = app.avg_hist.iter().cloned().collect();
    let peak = data.iter().cloned().max().unwrap_or(1).max(1);
    let spark = Sparkline::default()
        .block(Block::default().title(
            Span::styled(format!(" avg {:.1}%  peak {}% ", avg, peak),
                         Style::default().fg(pct_color(avg)))))
        .data(&data)
        .max(100)
        .style(Style::default().fg(pct_color(avg)));
    f.render_widget(spark, split[1]);
}

fn draw_ram(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let used  = app.mem.total.saturating_sub(app.mem.available);
    let mpct  = used as f64/app.mem.total as f64*100.0;
    let mc    = pct_color(mpct);
    let sused = app.mem.swap_total.saturating_sub(app.mem.swap_free);
    let spct  = if app.mem.swap_total>0 { sused as f64/app.mem.swap_total as f64*100.0 } else { 0.0 };

    let title = format!(" RAM  {} / {} ", fmt_mem(used), fmt_mem(app.mem.total));
    let block = Block::default().borders(Borders::ALL)
        .title(Span::styled(title, Style::default().fg(mc).add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 { return; }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Length(1); inner.height as usize])
        .split(inner);

    let bw = inner.width.saturating_sub(14).max(1) as usize;

    // RAM bar
    if rows.len() > 1 {
        let filled = ((mpct/100.0)*bw as f64).round() as usize;
        let bar = "█".repeat(filled) + &"░".repeat(bw-filled);
        let line = Line::from(vec![
            Span::styled("RAM  ", Style::default().fg(Color::DarkGray)),
            Span::styled(bar, Style::default().fg(mc)),
            Span::styled(format!(" {:5.1}%", mpct), Style::default().fg(mc)),
        ]);
        f.render_widget(Paragraph::new(line), rows[0]);
    }

    // SWAP bar
    if rows.len() > 2 {
        let sc = pct_color(spct);
        let line = if app.mem.swap_total > 0 {
            let filled = ((spct/100.0)*bw as f64).round() as usize;
            let bar = format!("█").repeat(filled) + &"░".repeat(bw-filled);
            Line::from(vec![
                Span::styled("SWAP ", Style::default().fg(Color::DarkGray)),
                Span::styled(bar, Style::default().fg(sc)),
                Span::styled(format!(" {:5.1}%", spct), Style::default().fg(sc)),
            ])
        } else {
            Line::from(vec![Span::styled("SWAP  none", Style::default().fg(Color::DarkGray))])
        };
        f.render_widget(Paragraph::new(line), rows[1]);
    }

    // Details
    let details = [
        ("Total", fmt_mem(app.mem.total),            Color::White),
        ("Used",  fmt_mem(used),                      mc),
        ("Free",  fmt_mem(app.mem.available),         Color::Green),
        ("Bufs",  fmt_mem(app.mem.buffers),           Color::DarkGray),
        ("Cache", fmt_mem(app.mem.cached),            Color::DarkGray),
        ("SwpT",  fmt_mem(app.mem.swap_total),        Color::DarkGray),
        ("SwpU",  fmt_mem(sused),                     pct_color(spct)),
    ];
    // Two columns of details
    let half = (details.len()+1)/2;
    for (i,(lbl,val,col)) in details.iter().enumerate() {
        let row_i = i%half + 3;
        let col_x = if i/half == 1 { inner.width/2 } else { 0 };
        if row_i >= rows.len() { break; }
        let line = Line::from(vec![
            Span::styled(format!("{:<6}",lbl), Style::default().fg(Color::DarkGray)),
            Span::styled(val.clone(), Style::default().fg(*col)),
        ]);
        let sub = Rect { x: inner.x+col_x, y: rows[row_i].y, width: inner.width/2, height: 1 };
        f.render_widget(Paragraph::new(line), sub);
    }
}

fn sparkline_widget<'a>(data: &'a VecDeque<u64>, label: &'a str, color: Color) -> Sparkline<'a> {
    Sparkline::default()
        .block(Block::default().title(Span::styled(label, Style::default().fg(color))))
        .data(data.as_slices().0)
        .style(Style::default().fg(color))
}

fn draw_net(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let title = format!(" NET  ▼ {}  ▲ {} ", fmt_rate(app.net_rx), fmt_rate(app.net_tx));
    let block = Block::default().borders(Borders::ALL)
        .title(Span::styled(title, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Min(0)])
        .split(inner);

    let rx_label = format!("▼ RX {}", fmt_rate(app.net_rx));
    let tx_label = format!("▲ TX {}", fmt_rate(app.net_tx));
    f.render_widget(sparkline_widget(&app.net_rx_hist, &rx_label, Color::Green),  split[0]);
    f.render_widget(sparkline_widget(&app.net_tx_hist, &tx_label, Color::Yellow), split[1]);
}

fn draw_disk(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let title = format!(" DISK  R {}  W {} ", fmt_rate(app.disk_r), fmt_rate(app.disk_w));
    let block = Block::default().borders(Borders::ALL)
        .title(Span::styled(title, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Min(0)])
        .split(inner);

    let r_label = format!("● Read {}", fmt_rate(app.disk_r));
    let w_label = format!("● Write {}", fmt_rate(app.disk_w));
    f.render_widget(sparkline_widget(&app.disk_r_hist, &r_label, Color::Cyan),    split[0]);
    f.render_widget(sparkline_widget(&app.disk_w_hist, &w_label, Color::Magenta), split[1]);
}

fn draw_gpu(f: &mut ratatui::Frame, app: &App, area: Rect) {
    if let Some(ref g) = app.gpu {
        let gc = pct_color(g.pct);
        let title = format!(" GPU {:.0}% ", g.pct);
        let block = Block::default().borders(Borders::ALL)
            .title(Span::styled(title, Style::default().fg(gc).add_modifier(Modifier::BOLD)));
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height < 1 { return; }

        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1),Constraint::Length(1),Constraint::Length(1),Constraint::Min(0)])
            .split(inner);

        f.render_widget(Paragraph::new(
            Span::styled(g.name.chars().take(inner.width as usize).collect::<String>(),
                         Style::default().fg(Color::DarkGray))), split[0]);

        let bw = inner.width as usize;
        let filled = ((g.pct/100.0)*bw as f64).round() as usize;
        let bar = "█".repeat(filled) + &"░".repeat(bw-filled);
        f.render_widget(Paragraph::new(Span::styled(bar, Style::default().fg(gc))), split[1]);

        let mem_str = format!("{}/{}", fmt_mem(g.mem_used*1024), fmt_mem(g.mem_total*1024));
        let temp_str = g.temp.map(|t| format!("  {:.0}°C", t)).unwrap_or_default();
        f.render_widget(Paragraph::new(
            Span::styled(format!("{}{}",mem_str,temp_str), Style::default().fg(Color::Cyan))), split[2]);

        if split.len() > 3 {
            f.render_widget(sparkline_widget(&app.gpu_hist, "", gc), split[3]);
        }
    }
}

fn draw_procs(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let procs = app.display_procs();
    let n = procs.len();

    let sort_arrow = if app.sort_asc { "▲" } else { "▼" };
    let search_ind = if !app.search.is_empty() { format!(" [/{}]", app.search) } else { String::new() };
    let title = format!(" PROCESSES{}  sort:{}{} {}  {} shown ",
        search_ind,
        match app.sort_col {
            SortCol::Cpu=>"CPU",SortCol::Mem=>"MEM",SortCol::Pid=>"PID",
            SortCol::Name=>"NAME",SortCol::User=>"USER",SortCol::Virt=>"VIRT",SortCol::Threads=>"THR",
        },
        sort_arrow, if app.sort_asc{"▲"}else{"▼"}, n);

    let block = Block::default().borders(Borders::ALL)
        .title(Span::styled(title, Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)));

    let cmd_w = (area.width as usize).saturating_sub(82).max(10);

    let header = Row::new(vec![
        "PID", "NAME", "USER", "ST", "CPU%", "MEM", "VIRT", "THR", "COMMAND",
    ]).style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let selected_style = Style::default().add_modifier(Modifier::REVERSED);

    let rows: Vec<Row> = procs.iter().map(|p| {
        let cc = pct_color(p.cpu);
        Row::new(vec![
            ratatui::text::Text::styled(format!("{}", p.pid),      Style::default().fg(Color::DarkGray)),
            ratatui::text::Text::styled(p.name.chars().take(15).collect::<String>(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
            ratatui::text::Text::styled(p.user.chars().take(10).collect::<String>(), Style::default().fg(Color::Cyan)),
            ratatui::text::Text::styled(state_label(p.state),       Style::default().fg(state_color(p.state))),
            ratatui::text::Text::styled(format!("{:>5.1}%", p.cpu), Style::default().fg(cc)),
            ratatui::text::Text::styled(fmt_mem(p.mem_kb),           Style::default().fg(Color::White)),
            ratatui::text::Text::styled(fmt_mem(p.virt_kb),          Style::default().fg(Color::DarkGray)),
            ratatui::text::Text::styled(format!("{}", p.threads),    Style::default().fg(Color::DarkGray)),
            ratatui::text::Text::styled(p.cmd.chars().take(cmd_w).collect::<String>(), Style::default().fg(Color::DarkGray)),
        ])
    }).collect();

    let widths = [
        Constraint::Length(7), Constraint::Length(16), Constraint::Length(11),
        Constraint::Length(5), Constraint::Length(7),  Constraint::Length(10),
        Constraint::Length(9), Constraint::Length(5),  Constraint::Min(10),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .highlight_style(selected_style)
        .highlight_symbol("▶ ");

    f.render_stateful_widget(table, area, &mut app.table_state);
}

// ═══════════════════════════════════════════════════════════════════════════════
// MAIN
// ═══════════════════════════════════════════════════════════════════════════════

fn main() -> std::io::Result<()> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    execute!(terminal.backend_mut(), crossterm::cursor::Show)?;
    result
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> std::io::Result<()> {
    let mut app = App::new();
    app.all_procs = read_procs_all();
    let mut last = Instant::now();

    loop {
        // Draw
        terminal.draw(|f| ui(f, &mut app))?;

        // Event loop — up to 1s
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let now = Instant::now();
            if now >= deadline { break; }
            let wait = (deadline-now).min(Duration::from_millis(50));
            if event::poll(wait)? {
                match event::read()? {
                    Event::Key(k) => {
                        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                            return Ok(());
                        }
                        let procs = app.display_procs();
                        let n = procs.len();
                        drop(procs);

                        match app.mode {
                            UiMode::Search => match k.code {
                                KeyCode::Esc       => { app.search.clear(); app.mode = UiMode::Normal; }
                                KeyCode::Enter     => { app.mode = UiMode::Normal; }
                                KeyCode::Backspace => { app.search.pop(); }
                                KeyCode::Char(c)   => { app.search.push(c); app.table_state.select(Some(0)); }
                                _ => {}
                            },
                            UiMode::Kill => match k.code {
                                KeyCode::Char('y') | KeyCode::Enter => {
                                    let procs = app.display_procs();
                                    if let Some(p) = procs.get(app.selected_idx()) {
                                        let pid = p.pid; let sig = app.kill_sig;
                                        drop(procs);
                                        match std::process::Command::new("kill")
                                            .args([format!("-{}",sig), pid.to_string()]).output() {
                                            Ok(o) if o.status.success() =>
                                                app.set_status(format!("Sent signal {} to PID {}",sig,pid)),
                                            Ok(o) => app.set_status(format!("Error: {}",
                                                String::from_utf8_lossy(&o.stderr).trim())),
                                            Err(e) => app.set_status(format!("Error: {}",e)),
                                        }
                                    }
                                    app.mode = UiMode::Normal;
                                }
                                KeyCode::F(9)                   => app.mode = UiMode::Signal,
                                KeyCode::Char('n')|KeyCode::Esc => app.mode = UiMode::Normal,
                                _ => {}
                            },
                            UiMode::Signal => match k.code {
                                KeyCode::Esc   => app.mode = UiMode::Normal,
                                KeyCode::Enter => {
                                    let procs = app.display_procs();
                                    if let Some(p) = procs.get(app.selected_idx()) {
                                        let pid = p.pid; let sig = app.kill_sig;
                                        drop(procs);
                                        match std::process::Command::new("kill")
                                            .args([format!("-{}",sig), pid.to_string()]).output() {
                                            Ok(o) if o.status.success() =>
                                                app.set_status(format!("Sent signal {} to PID {}",sig,pid)),
                                            _ => app.set_status("Error sending signal"),
                                        }
                                    }
                                    app.mode = UiMode::Normal;
                                }
                                KeyCode::Char(c) if c.is_ascii_digit() => {
                                    let d = c as i32 - '0' as i32;
                                    app.kill_sig = if app.kill_sig < 10 { app.kill_sig*10+d } else { d };
                                }
                                _ => {}
                            },
                            UiMode::Normal => match k.code {
                                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                                KeyCode::Char('/') => { app.mode = UiMode::Search; app.search.clear(); }
                                KeyCode::Char('k') => { if n>0 { app.kill_sig=15; app.mode=UiMode::Kill; } }
                                KeyCode::F(9)      => { if n>0 { app.kill_sig=15; app.mode=UiMode::Signal; } }
                                KeyCode::Char('g') => app.show_graphs = !app.show_graphs,
                                KeyCode::Char('r') => app.sort_asc = !app.sort_asc,
                                KeyCode::Char('c') => { app.sort_col=SortCol::Cpu;    app.sort_asc=false; }
                                KeyCode::Char('m') => { app.sort_col=SortCol::Mem;    app.sort_asc=false; }
                                KeyCode::Char('p') => { app.sort_col=SortCol::Pid;    app.sort_asc=true;  }
                                KeyCode::Char('n') => { app.sort_col=SortCol::Name;   app.sort_asc=true;  }
                                KeyCode::Char('u') => { app.sort_col=SortCol::User;   app.sort_asc=true;  }
                                KeyCode::Char('v') => { app.sort_col=SortCol::Virt;   app.sort_asc=false; }
                                KeyCode::Char('?') => app.set_status(
                                    "[/]search [k]kill [F9]sig [g]graphs [r]rev [c/m/p/n/u/v]sort [q]quit"),
                                KeyCode::Up       => app.move_up(1),
                                KeyCode::Down     => app.move_down(1, n),
                                KeyCode::PageUp   => app.move_up(20),
                                KeyCode::PageDown => app.move_down(20, n),
                                KeyCode::Home     => app.table_state.select(Some(0)),
                                KeyCode::End      => app.table_state.select(Some(n.saturating_sub(1))),
                                _ => {}
                            },
                        }
                    }
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollUp   => app.move_up(1),
                        MouseEventKind::ScrollDown => {
                            let n = app.display_procs().len();
                            app.move_down(1, n);
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            // ratatui's TableState handles selection via render,
                            // so just move selection to clicked row approximately
                            let n = app.display_procs().len();
                            let clicked = m.row as usize;
                            if clicked < n { app.table_state.select(Some(clicked)); }
                        }
                        _ => {}
                    },
                    Event::Resize(_, _) => { terminal.autoresize()?; }
                    _ => {}
                }
                terminal.draw(|f| ui(f, &mut app))?;
            }
        }

        // Refresh data
        let dt = last.elapsed().as_secs_f64();
        app.refresh(dt);
        last = Instant::now();
    }
}
