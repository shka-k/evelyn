use std::path::Path;
use std::process::Command;

use super::socket_probe;
use super::{ProcInfo, Processes};

pub struct Target {
    pub session: String,
}

/// Detection flow:
///   1. Find a zellij *client* process in our PTY subtree. Pane shells
///      live under the daemonized server (ppid=1), unreachable from
///      Evelyn's child PID, so env-based detection on descendants is a
///      dead end.
///   2. Ask `socket_probe` which named unix socket the client is
///      connected to. zellij server sockets live at
///      `…/zellij-<uid>/contract_version_<N>/<session-name>`, so the
///      socket path's last segment is the session name.
///   3. If socket probing isn't available (other platforms) or returns
///      nothing useful, fall back to enumerating `zellij --server`
///      processes by argv. That only disambiguates the single-server
///      case, but it works without any OS-specific socket inspection.
pub fn detect(scan: &[u32], procs: &Processes) -> Option<Target> {
    let client = find_client(scan, procs)?;
    if let Some(path) = socket_probe::connected_socket_path(client) {
        return Some(Target {
            session: session_from_socket_path(&path),
        });
    }
    let servers = list_servers(procs);
    if servers.len() == 1 {
        return Some(Target {
            session: session_from_socket_path(&servers[0].1),
        });
    }
    None
}

fn find_client(scan: &[u32], procs: &Processes) -> Option<u32> {
    scan.iter()
        .copied()
        .find(|&pid| procs.command(pid).is_some_and(is_client_cmd))
}

fn is_client_cmd(cmd: &str) -> bool {
    let prog = cmd.split_whitespace().next().unwrap_or("");
    let base = prog.rsplit('/').next().unwrap_or(prog);
    // A bare `zellij` (no --server) is a client. The server process
    // has `--server <socket-path>` in its argv.
    base == "zellij" && !cmd.split_whitespace().any(|t| t == "--server")
}

fn list_servers(procs: &Processes) -> Vec<(u32, String)> {
    procs
        .iter()
        .filter_map(|(pid, info)| socket_from_server_argv(info).map(|p| (pid, p)))
        .collect()
}

fn socket_from_server_argv(info: &ProcInfo) -> Option<String> {
    let mut tokens = info.command.split_whitespace();
    let prog = tokens.next()?;
    let base = prog.rsplit('/').next().unwrap_or(prog);
    if base != "zellij" {
        return None;
    }
    let mut tokens = info.command.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok == "--server" {
            return tokens.next().map(str::to_string);
        }
    }
    None
}

fn session_from_socket_path(path: &str) -> String {
    path.rsplit('/').next().unwrap_or("").to_string()
}

/// `zellij action dump-screen --full --path <FILE>` writes the targeted
/// session's focused pane (including scrollback) directly to disk.
/// `--path` is a flag in zellij 0.44+, not a positional argument —
/// passing the path positionally fails with exit 2 and zellij prints to
/// stdout instead.
pub fn dump(target: &Target, dest: &Path) -> bool {
    let mut cmd = Command::new("zellij");
    cmd.args([
        "--session",
        &target.session,
        "action",
        "dump-screen",
        "--full",
        "--path",
    ])
    .arg(dest);
    match cmd.status() {
        Ok(s) if s.success() => true,
        Ok(s) => {
            eprintln!("[evelyn] zellij dump-screen exited with {s}; falling back to Evelyn buffer");
            false
        }
        Err(e) => {
            eprintln!("[evelyn] zellij dump-screen failed: {e}; falling back to Evelyn buffer");
            false
        }
    }
}
