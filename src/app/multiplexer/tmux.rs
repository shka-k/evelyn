use std::path::Path;
use std::process::Command;

use super::Processes;
use super::socket_probe;

pub struct Target {
    /// Socket path the client is connected to (matches what `tmux -S`
    /// expects).
    pub socket: String,
    /// `-t` argument for `capture-pane` — a `pane_id` like `%5` when
    /// we could resolve it through `list-clients`, falling back to
    /// `=session` form when only the session name is known.
    pub target_arg: String,
    /// Absolute path to the tmux binary, captured from the running
    /// client's argv. Evelyn.app launched from Finder inherits a
    /// minimal PATH that does NOT include mise/asdf/homebrew-non-default
    /// install dirs, so `Command::new("tmux")` would fail with ENOENT
    /// even though the user is actively using tmux. Spawning the same
    /// binary by absolute path sidesteps the PATH problem entirely.
    pub binary: String,
}

/// Same shape as zellij detection: pane shells live under the
/// daemonized tmux server (ppid=1), so we find the *client* in our
/// subtree and resolve which pane it's focused on via the unix socket
/// they share.
pub fn detect(scan: &[u32], procs: &Processes) -> Option<Target> {
    let client_pid = find_client(scan, procs)?;
    let client_cmd = procs.command(client_pid).unwrap_or("");
    let binary = resolve_binary(client_pid, client_cmd);
    let socket = resolve_socket(client_pid, client_cmd)?;
    let target_arg = resolve_target(&socket, client_pid, client_cmd, &binary)?;
    Some(Target {
        socket,
        target_arg,
        binary,
    })
}

/// See zellij.rs `binary_path_for` for why argv0 alone isn't enough.
/// tmux is the case where this matters most: shells like bash exec it
/// with `argv0 = "tmux"`, not the absolute path, and Evelyn.app's
/// launchd-inherited PATH doesn't include mise/asdf/homebrew-non-default
/// dirs, so `Command::new("tmux")` would fail with ENOENT and the
/// dump would fall back to Evelyn's full buffer (the symptom that
/// surfaced this).
fn resolve_binary(pid: u32, cmd: &str) -> String {
    if let Some(path) = socket_probe::executable_path(pid, "tmux") {
        return path;
    }
    if let Some(argv0) = cmd.split_whitespace().next()
        && argv0.starts_with('/')
    {
        return argv0.to_string();
    }
    "tmux".to_string()
}

fn find_client(scan: &[u32], procs: &Processes) -> Option<u32> {
    scan.iter()
        .copied()
        .find(|&pid| procs.command(pid).is_some_and(is_client_cmd))
}

fn is_client_cmd(cmd: &str) -> bool {
    let prog = cmd.split_whitespace().next().unwrap_or("");
    let base = prog.rsplit('/').next().unwrap_or(prog);
    base == "tmux"
}

/// Prefer an explicit `-S <path>` from the client's argv (cheap), then
/// fall back to the platform socket probe. `-L <name>` is intentionally
/// not honored here — it gives a socket *name* but not the full path,
/// and resolving the path needs the tmux tmp dir and uid, which is
/// exactly the OS-specific juggling we delegate to `socket_probe`.
fn resolve_socket(pid: u32, cmd: &str) -> Option<String> {
    if let Some(path) = arg_value(cmd, "-S") {
        return Some(path);
    }
    socket_probe::connected_socket_path(pid)
}

/// Ask the tmux server which pane *our specific client* is focused on.
/// Two-step lookup: get our client's tty by PID, then run
/// `display-message` as that client so the `#{pane_id}` evaluation
/// sits in the right session/window/pane context.
fn resolve_target(socket: &str, client_pid: u32, cmd: &str, binary: &str) -> Option<String> {
    if let Some(tty) = client_tty_for_pid(socket, client_pid, binary)
        && let Some(pane) = pane_for_client_tty(socket, &tty, binary)
    {
        return Some(pane);
    }
    if let Some(session) = arg_value(cmd, "-t") {
        return Some(format!("={session}"));
    }
    any_session_on_socket(socket, binary).map(|s| format!("={s}"))
}

fn client_tty_for_pid(socket: &str, client_pid: u32, binary: &str) -> Option<String> {
    let out = Command::new(binary)
        .args([
            "-S",
            socket,
            "list-clients",
            "-F",
            "#{client_pid}\t#{client_tty}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<(u32, String)> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let pid = fields.next()?.parse::<u32>().ok()?;
            let tty = fields.next()?.to_string();
            Some((pid, tty))
        })
        .collect();
    if let Some((_, tty)) = rows.iter().find(|(pid, _)| *pid == client_pid) {
        return Some(tty.clone());
    }
    // Only fall back to the sole client when ambiguity is impossible.
    // With multiple attached clients we'd otherwise dump the wrong
    // window's pane.
    if rows.len() == 1 {
        return Some(rows.into_iter().next().unwrap().1);
    }
    None
}

fn pane_for_client_tty(socket: &str, client_tty: &str, binary: &str) -> Option<String> {
    // `display-message`'s `-c` selects the target-client (its tty);
    // `-t` is target-pane, which is what we're trying to discover and
    // would defeat the lookup. The format goes as a positional arg.
    let out = Command::new(binary)
        .args([
            "-S",
            socket,
            "display-message",
            "-p",
            "-c",
            client_tty,
            "#{pane_id}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let pane = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if pane.is_empty() { None } else { Some(pane) }
}

fn any_session_on_socket(socket: &str, binary: &str) -> Option<String> {
    let out = Command::new(binary)
        .args(["-S", socket, "list-sessions", "-F", "#S"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(str::to_string)
}

fn arg_value(cmd: &str, flag: &str) -> Option<String> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    for w in tokens.windows(2) {
        if w[0] == flag {
            return Some(w[1].to_string());
        }
    }
    None
}

/// `tmux -S <socket> capture-pane -p -S - -t <pane_id|=session>`
/// prints the focused pane's history (full scrollback via `-S -`) to
/// stdout. We write that straight to `dest`.
pub fn dump(target: &Target, dest: &Path) -> bool {
    let out = match Command::new(&target.binary)
        .args([
            "-S",
            &target.socket,
            "capture-pane",
            "-p",
            "-S",
            "-",
            "-t",
            &target.target_arg,
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            eprintln!(
                "[evelyn] tmux capture-pane exited with {}; falling back to Evelyn buffer",
                o.status
            );
            return false;
        }
        Err(e) => {
            eprintln!("[evelyn] tmux capture-pane failed: {e}; falling back to Evelyn buffer");
            return false;
        }
    };
    if let Err(e) = std::fs::write(dest, &out.stdout) {
        eprintln!("[evelyn] write tmux dump failed: {e}");
        return false;
    }
    true
}
