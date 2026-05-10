# sysmon

A fast, beautiful, and lightweight system monitor for Linux, written in Rust.

Inspired by tools like `htop`, `glances`, and `bottom`, but with a clean modern TUI, smooth graphs, and excellent performance.

## Features

- **Real-time graphs** — CPU (per-core + average), RAM, Swap, Network, Disk I/O, GPU
- **Process viewer** with tree mode, search, sorting, and mouse support
- **GPU support** — NVIDIA (`nvidia-smi`) and AMD (via sysfs)
- **Efficient rendering** — custom double-buffered diff renderer (very low CPU usage)
- **Interactive** — kill processes, send signals, change sort, toggle graphs/tree view
- **Minimal dependencies** — no heavy crates, works great on Fedora and other Linux distros

Perfect for Fedora Workstation, Silverblue, or any modern Linux desktop.

## Quick Install

bash
# Build from source
git clone https://github.com/isaactheguy244/sysmon.git
cd sysmon
cargo build --release
sudo install -Dm755 target/release/sysmon /usr/local/bin/sysmon
