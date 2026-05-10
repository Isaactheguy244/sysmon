# sysmon

**A fast, beautiful, and lightweight Rust TUI system monitor for Linux.**

Inspired by `htop`, `glances`, and `bottom`, with smooth real-time graphs and a clean modern interface.

<!-- Add your screenshot here -->
<!-- ![sysmon demo](screenshot.png) -->

## Features

- Real-time graphs — CPU (per-core + average), RAM, Swap, Network, Disk I/O, GPU
- Process viewer with tree mode, search, sorting, and mouse support
- GPU support — NVIDIA (`nvidia-smi`) and AMD (via sysfs)
- Very efficient custom double-buffered renderer (low CPU usage)
- Interactive — kill processes, send signals, toggle graphs/tree view
- Minimal dependencies — works great on Fedora Workstation, Silverblue, Nobara, etc.

## Quick Install

```bash
git clone https://github.com/Isaactheguy244/sysmon.git
cd sysmon
cargo build --release
sudo install -Dm755 target/release/sysmon /usr/local/bin/sysmon
