//! File IO built on top of `exec::ExecSession` — reading a config file,
//! listing a directory (for the file browser), and safely writing a
//! fixed-up config back (backup first, atomic rename, same shape as
//! `sslcert::engine::write_remote_file`). Every operation goes through
//! shell commands rather than `std::fs` even for `Target::Local`, so the
//! Local and Remote paths are the exact same code — see
//! `exec::ExecSession::run`.

use super::exec::{shell_quote, ExecSession};

pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

/// Lists `path` (must be an absolute, already-resolved directory — no `~`,
/// see the caller in `tui::config_check_screen` for why). `-p` marks
/// directories with a trailing `/` so a single round trip gives us both
/// the name and the type; `-A` skips `.`/`..` (the browser adds its own
/// `..` row when appropriate).
pub fn list_dir(session: &ExecSession, path: &str) -> Result<Vec<Entry>, String> {
    let cmd = format!("ls -1Ap {} 2>&1", shell_quote(path));
    let (out, _) = session.run_checked_sudo(&cmd)?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| match l.strip_suffix('/') {
            Some(name) => Entry { name: name.to_string(), is_dir: true },
            None => Entry { name: l.to_string(), is_dir: false },
        })
        .collect())
}

pub fn read_file(session: &ExecSession, path: &str) -> Result<String, String> {
    let cmd = format!("cat {} 2>&1", shell_quote(path));
    session.run_checked_sudo(&cmd).map(|(out, _)| out)
}

fn now_stamp(session: &ExecSession) -> String {
    session.run("date -u +%Y%m%d%H%M%S").ok().map(|(out, _, _)| out.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "0".to_string())
}

/// Backs up `path` (if it exists) to `<path>.atk-bak-<timestamp>`, then
/// atomically replaces its content with `content`. Returns the backup path
/// so the caller can tell the user where their original file went. Mirrors
/// `sslcert::engine`'s write pattern: write to a temp name in the same
/// directory, then `mv` over the real path, so a crash mid-write never
/// leaves a half-written config in place.
pub fn write_file_with_backup(session: &ExecSession, path: &str, content: &str) -> Result<String, String> {
    let backup = format!("{path}.atk-bak-{}", now_stamp(session));
    let backup_cmd = format!("[ -f {p} ] && cp -p {p} {b} || true", p = shell_quote(path), b = shell_quote(&backup));
    session.run_checked_sudo(&backup_cmd)?;

    let dir = std::path::Path::new(path).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let mkdir_part = if dir.is_empty() || dir == "/" { String::new() } else { format!("mkdir -p {} && ", shell_quote(&dir)) };
    let write_cmd = format!(
        "{mkdir_part}printf '%s' {} > {}.atk-tmp && mv {}.atk-tmp {}",
        shell_quote(content),
        shell_quote(path),
        shell_quote(path),
        shell_quote(path)
    );
    session.run_checked_sudo(&format!("bash -c {}", shell_quote(&write_cmd)))?;
    Ok(backup)
}

pub fn parent_dir(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(idx) => trimmed[..idx].to_string(),
        None => "/".to_string(),
    }
}

pub fn join_dir(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}
