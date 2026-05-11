//! Identify which named unix socket a given client process is
//! connected to. Used by `tmux` and `zellij` detection to pin down
//! *which* multiplexer instance owns the Evelyn window — the
//! per-pane env vars (`TMUX`, `ZELLIJ_SESSION_NAME`) live on pane
//! shells that are daemonized off Evelyn's process subtree, so they're
//! unreachable; the unix-socket connection from the client process is
//! the only link that stays inside our tree.
//!
//! The implementation is platform-specific because there's no portable
//! way to ask the kernel "what's the path of the peer's bound socket?".
//! Each platform has its own backdoor:
//! - macOS: parse `lsof -aU` output (DEVICE / peer-ref kernel addresses)
//! - Linux: read `/proc/<pid>/net/unix` + `/proc/net/unix` (not yet
//!   implemented — would slot in alongside the macOS module)

/// Given a process connected to a unix domain socket, return the path
/// of the named socket on the other end (the server's bind path).
/// Returns `None` if probing isn't supported on this platform or no
/// connection could be resolved.
pub fn connected_socket_path(client_pid: u32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        macos::connected_socket_path(client_pid)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = client_pid;
        None
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashSet;
    use std::process::Command;

    // macOS `lsof` leaves the NODE column blank for unix sockets, so
    // data rows have 8 whitespace-separated tokens (the header has 9).
    // Column indices below are for the collapsed `split_whitespace`
    // form, not the header positions.
    const COL_TYPE: usize = 4;
    const COL_DEVICE: usize = 5;
    const COL_NAME: usize = 7;

    pub fn connected_socket_path(client_pid: u32) -> Option<String> {
        let text = lsof_unix_for(client_pid)?;
        let addrs = client_addrs(&text);
        if addrs.is_empty() {
            return None;
        }
        find_named_peer(&addrs)
    }

    /// Collect both the DEVICE column and any `->0x…` peer refs from
    /// the client's lsof output. lsof labels the server end of a
    /// connection inconsistently — sometimes the server-side accepted
    /// fd shares the *client's* device address, sometimes it shares
    /// the *peer* address. Matching either covers both shapes.
    fn client_addrs(lsof_text: &str) -> HashSet<String> {
        let mut set = HashSet::new();
        for line in lsof_text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() <= COL_DEVICE || cols[COL_TYPE] != "unix" {
                continue;
            }
            set.insert(cols[COL_DEVICE].to_string());
            if let Some(name) = cols.get(COL_NAME)
                && let Some(stripped) = name.strip_prefix("->")
            {
                set.insert(stripped.to_string());
            }
        }
        set
    }

    /// Scan every unix socket in the system for one whose DEVICE
    /// matches an entry from the client's address set and whose NAME
    /// is a real path (not a `->0x…` peer ref). Kernel socket
    /// addresses are unique, so a device match is sufficient to
    /// identify the server end — no need to filter by socket-path
    /// substring (which would miss `tmux -S /custom/path` setups).
    fn find_named_peer(addrs: &HashSet<String>) -> Option<String> {
        let out = Command::new("lsof").args(["-U"]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() <= COL_DEVICE || cols[COL_TYPE] != "unix" {
                continue;
            }
            if !addrs.contains(cols[COL_DEVICE]) {
                continue;
            }
            let name = cols.get(COL_NAME).copied().unwrap_or("");
            if !name.is_empty() && !name.starts_with("->") {
                return Some(name.to_string());
            }
        }
        None
    }

    fn lsof_unix_for(pid: u32) -> Option<String> {
        let out = Command::new("lsof")
            .args(["-p", &pid.to_string(), "-aU"])
            .output()
            .ok()?;
        // `lsof` returns exit 1 when no matches; treat any stdout as
        // usable so callers can still parse partial output.
        if out.stdout.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}
