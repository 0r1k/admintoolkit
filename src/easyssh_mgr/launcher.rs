//! Talking to the real `ssh` binary: building a copy-pasteable command line,
//! a lightweight TCP reachability check, and background port forwarding.
//!
//! Port forwarding is declarative rather than ad-hoc: `LocalForward`/
//! `RemoteForward`/`DynamicForward` are already core fields on the server
//! profile (written straight into its `~/.ssh/config` block), so starting a
//! forward is just `ssh -N <alias>` — the same directives ssh would use for
//! an interactive connection apply automatically.

use std::collections::HashMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::config::Server;

pub fn build_ssh_command(server: &Server) -> String {
    let mut parts = vec!["ssh".to_string()];
    if !server.proxy_jump.is_empty() {
        parts.push("-J".to_string());
        parts.push(server.proxy_jump.clone());
    }
    for f in &server.identity_files {
        parts.push("-i".to_string());
        parts.push(f.clone());
    }
    if server.port != 0 && server.port != 22 {
        parts.push("-p".to_string());
        parts.push(server.port.to_string());
    }
    if server.forward_agent.eq_ignore_ascii_case("yes") {
        parts.push("-A".to_string());
    }
    for f in &server.local_forward {
        parts.push("-L".to_string());
        parts.push(f.clone());
    }
    for f in &server.remote_forward {
        parts.push("-R".to_string());
        parts.push(f.clone());
    }
    for f in &server.dynamic_forward {
        parts.push("-D".to_string());
        parts.push(f.clone());
    }
    if !server.connect_timeout.is_empty() {
        parts.push("-o".to_string());
        parts.push(format!("ConnectTimeout={}", server.connect_timeout));
    }
    if !server.strict_host_key_checking.is_empty() {
        parts.push("-o".to_string());
        parts.push(format!("StrictHostKeyChecking={}", server.strict_host_key_checking));
    }
    for (k, v) in &server.extra {
        parts.push("-o".to_string());
        parts.push(format!("{k}={v}"));
    }
    let target = server.effective_host().to_string();
    parts.push(if server.user.is_empty() { target } else { format!("{}@{target}", server.user) });
    parts.join(" ")
}

/// TCP-connects to the server's resolved host:port (falling back to the
/// alias itself and port 22) with a 3s timeout — good enough to answer "is
/// something listening", without shelling out to `ssh -G` like upstream
/// does, since we already have host/port parsed from the config.
pub fn ping(server: &Server) -> Result<Duration, String> {
    let host = server.effective_host();
    let port = if server.port != 0 { server.port } else { 22 };
    let start = Instant::now();
    let addr = (host, port).to_socket_addrs().map_err(|e| e.to_string())?.next().ok_or("could not resolve host")?;
    TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|e| e.to_string())?;
    Ok(start.elapsed())
}

static FORWARDS: Mutex<Option<HashMap<String, Vec<Child>>>> = Mutex::new(None);

/// Starts `ssh -N <alias>` in the background, tracked by alias so it can be
/// stopped later. Returns the child PID.
pub fn start_forward(alias: &str) -> Result<u32, String> {
    let child = Command::new("ssh")
        .args(["-N", alias])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let pid = child.id();
    let mut guard = FORWARDS.lock().map_err(|_| "forwards lock poisoned".to_string())?;
    guard.get_or_insert_with(HashMap::new).entry(alias.to_string()).or_default().push(child);
    Ok(pid)
}

pub fn stop_forwarding(alias: &str) -> Result<(), String> {
    let mut guard = FORWARDS.lock().map_err(|_| "forwards lock poisoned".to_string())?;
    if let Some(map) = guard.as_mut() {
        if let Some(children) = map.remove(alias) {
            for mut child in children {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    Ok(())
}

pub fn is_forwarding(alias: &str) -> bool {
    let mut guard = match FORWARDS.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    if let Some(map) = guard.as_mut() {
        if let Some(children) = map.get_mut(alias) {
            children.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
            if children.is_empty() {
                map.remove(alias);
            } else {
                return true;
            }
        }
    }
    false
}
