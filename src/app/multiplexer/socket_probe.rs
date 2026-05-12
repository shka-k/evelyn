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

/// Resolve the absolute path of a running process's main executable.
/// Used to spawn the same multiplexer binary the user is interacting
/// with — `Command::new("zellij"|"tmux")` is unreliable because
/// Evelyn.app launched from Finder inherits a stripped PATH, and
/// argv0 in `ps` isn't always an absolute path (tmux/bash leave it
/// as the bare name when invoked without one).
pub fn executable_path(pid: u32, expected_basename: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        macos::executable_path(pid, expected_basename)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (pid, expected_basename);
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
    const COL_PID: usize = 1;
    const COL_TYPE: usize = 4;
    const COL_DEVICE: usize = 5;
    const COL_NAME: usize = 7;

    pub fn connected_socket_path(client_pid: u32) -> Option<String> {
        let text = lsof_unix_for(client_pid)?;
        let addrs = client_addrs(&text);
        if addrs.is_empty() {
            return None;
        }
        find_named_peer(&addrs, client_pid)
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

    /// Scan every unix socket in the system to find the server end of
    /// the client's connection, then return that server's named bind
    /// path. Two shapes need to be handled because macOS lsof labels
    /// accept()ed connections inconsistently between multiplexers:
    ///
    /// - zellij: the server keeps the listening socket's kernel DEVICE
    ///   visible from the client side too (the client's peer ref
    ///   matches the listening socket's DEVICE). A direct DEVICE-match
    ///   row whose NAME is a path resolves it in one step.
    /// - tmux: `accept()` returns a brand-new socket pair, so the
    ///   client's peer ref points at the accepted-side socket, NOT the
    ///   listening one. We have to first locate the *PID* that owns
    ///   the matching accepted socket, then look up the listening
    ///   (named-path) socket owned by that same PID.
    fn find_named_peer(addrs: &HashSet<String>, client_pid: u32) -> Option<String> {
        let out = Command::new("lsof").args(["-U"]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let rows: Vec<Vec<&str>> = text
            .lines()
            .skip(1)
            .map(|l| l.split_whitespace().collect())
            .filter(|cols: &Vec<&str>| cols.len() > COL_DEVICE && cols[COL_TYPE] == "unix")
            .collect();

        let mut peer_pid: Option<u32> = None;
        for cols in &rows {
            let row_pid: u32 = match cols.get(COL_PID).and_then(|s| s.parse().ok()) {
                Some(p) => p,
                None => continue,
            };
            if row_pid == client_pid {
                continue;
            }
            if !addrs.contains(cols[COL_DEVICE]) {
                continue;
            }
            // Direct hit: the row owning a matching DEVICE already has
            // the bind path as its NAME (zellij shape).
            let name = cols.get(COL_NAME).copied().unwrap_or("");
            if !name.is_empty() && !name.starts_with("->") {
                return Some(name.to_string());
            }
            // Otherwise remember the PID so we can pull its listening
            // socket below (tmux shape).
            peer_pid = Some(row_pid);
            break;
        }

        let pid = peer_pid?;
        for cols in &rows {
            let row_pid: u32 = match cols.get(COL_PID).and_then(|s| s.parse().ok()) {
                Some(p) => p,
                None => continue,
            };
            if row_pid != pid {
                continue;
            }
            let name = cols.get(COL_NAME).copied().unwrap_or("");
            if !name.is_empty() && !name.starts_with("->") {
                return Some(name.to_string());
            }
        }
        None
    }

    /// Walk `lsof -p <pid>` for the main executable mapping. The
    /// process's own binary appears as an `FD=txt, TYPE=REG` row, but
    /// every loaded dylib is also `txt/REG`, so we can't just take the
    /// first match — we filter by basename against what the caller
    /// expects (e.g. `zellij`, `tmux`). lsof has no `-o exe` style
    /// shortcut and macOS doesn't expose `/proc/<pid>/exe`, so this
    /// scan is the cheapest reliable option short of FFI to `proc_pidpath`.
    pub fn executable_path(pid: u32, expected_basename: &str) -> Option<String> {
        let out = Command::new("lsof")
            .args(["-p", &pid.to_string()])
            .output()
            .ok()?;
        if out.stdout.is_empty() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // FD column is index 3, TYPE column is index 4 in `lsof -p`
            // output (no -U filter, so the layout matches the standard
            // header: COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME).
            if cols.get(3) != Some(&"txt") || cols.get(4) != Some(&"REG") {
                continue;
            }
            // Last column is NAME (the file path).
            let path = match cols.last() {
                Some(p) => *p,
                None => continue,
            };
            let base = path.rsplit('/').next().unwrap_or(path);
            if base == expected_basename {
                return Some(path.to_string());
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
