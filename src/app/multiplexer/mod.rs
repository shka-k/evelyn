//! Detect a terminal multiplexer running inside our PTY so the buffer
//! dump can ask the multiplexer for *its* scrollback instead of capturing
//! Evelyn's rendered view of the multiplexer chrome (status bar, pane
//! dividers, single visible pane).
//!
//! One submodule per multiplexer; this file holds the shared process-tree
//! traversal and dispatch order. The traversal is non-trivial because
//! both zellij and tmux daemonize their server, which means **pane
//! shells are not descendants of our PTY's child**. The shape is:
//!
//! ```text
//! Evelyn ── shell (Evelyn's child) ── multiplexer client ──┐
//!                                                          ↓ socket
//! PID 1 ──── multiplexer server (daemonized) ── pane shells
//! ```
//!
//! So `ZELLIJ_SESSION_NAME` / `TMUX` env vars on pane shells live in a
//! totally separate subtree from ours. We instead find the *client*
//! process in our descendants and match it to its server via the unix
//! socket they share.

mod socket_probe;
mod tmux;
mod zellij;

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// Try to dump the active multiplexer pane's scrollback to `dest`.
/// Returns `true` only on a successful dump — callers fall back to
/// Evelyn's own buffer when this returns `false`. Detection order is
/// zellij → tmux; in practice only one of the two is in the tree.
pub fn dump_active_buffer(root_pid: u32, dest: &Path) -> bool {
    let Some(procs) = Processes::collect() else {
        return false;
    };
    let mut scan = vec![root_pid];
    scan.extend(procs.descendants(root_pid));

    if let Some(target) = zellij::detect(&scan, &procs) {
        return zellij::dump(&target, dest);
    }
    if let Some(target) = tmux::detect(&scan, &procs) {
        return tmux::dump(&target, dest);
    }
    false
}

/// Snapshot of `ps -A` indexed by PID, used by submodules to walk the
/// process tree and inspect command lines without re-shelling out per
/// process.
pub(super) struct Processes {
    by_pid: HashMap<u32, ProcInfo>,
}

pub(super) struct ProcInfo {
    pub ppid: u32,
    pub command: String,
}

impl Processes {
    fn collect() -> Option<Self> {
        let out = Command::new("ps")
            .args(["-A", "-ww", "-o", "pid=,ppid=,command="])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut by_pid = HashMap::new();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let Some(pid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let Some(ppid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let command = parts.collect::<Vec<_>>().join(" ");
            by_pid.insert(pid, ProcInfo { ppid, command });
        }
        Some(Self { by_pid })
    }

    /// PIDs reachable from `root` via parent→child edges, excluding
    /// `root` itself. Callers that want "self + descendants" should
    /// prepend the root manually.
    pub fn descendants(&self, root: u32) -> Vec<u32> {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (&pid, info) in &self.by_pid {
            children.entry(info.ppid).or_default().push(pid);
        }
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            if let Some(kids) = children.get(&pid) {
                for &kid in kids {
                    out.push(kid);
                    stack.push(kid);
                }
            }
        }
        out
    }

    pub fn command(&self, pid: u32) -> Option<&str> {
        self.by_pid.get(&pid).map(|i| i.command.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &ProcInfo)> {
        self.by_pid.iter().map(|(k, v)| (*k, v))
    }
}
