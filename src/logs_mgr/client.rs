//! Fetches logs over SSH: the systemd journal via `journalctl`, or a plain
//! file under `/var/log` (or anywhere else) via `tail`. Everything runs as
//! a single read-only remote command — nothing here ever writes to the
//! target server.

use super::config::ConnectionWithSecrets;
use crate::ssh_exec::{escape_single_quotes, Credentials, SshSession};

pub fn connect(cfg: &ConnectionWithSecrets) -> Result<SshSession, String> {
    let creds = Credentials {
        user: cfg.ssh_user.clone(),
        password: cfg.ssh_password.clone(),
        private_key_path: cfg.ssh_key_path.clone(),
    };
    SshSession::connect(&cfg.host, &cfg.ssh_port, &creds)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Journal,
    File,
}

/// Syslog severity levels, most to least severe, plus `All` (no filter).
/// Selecting one shows that level and everything more severe — matches
/// `journalctl -p`'s own semantics.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    All,
    Emerg,
    Alert,
    Crit,
    Err,
    Warning,
    Notice,
    Info,
    Debug,
}

pub const PRIORITIES: &[Priority] =
    &[Priority::All, Priority::Emerg, Priority::Alert, Priority::Crit, Priority::Err, Priority::Warning, Priority::Notice, Priority::Info, Priority::Debug];

impl Priority {
    pub fn label(&self) -> &'static str {
        match self {
            Priority::All => "All",
            Priority::Emerg => "Emerg",
            Priority::Alert => "Alert",
            Priority::Crit => "Crit",
            Priority::Err => "Error",
            Priority::Warning => "Warning",
            Priority::Notice => "Notice",
            Priority::Info => "Info",
            Priority::Debug => "Debug",
        }
    }

    fn journalctl_arg(&self) -> Option<&'static str> {
        match self {
            Priority::All => None,
            Priority::Emerg => Some("emerg"),
            Priority::Alert => Some("alert"),
            Priority::Crit => Some("crit"),
            Priority::Err => Some("err"),
            Priority::Warning => Some("warning"),
            Priority::Notice => Some("notice"),
            Priority::Info => Some("info"),
            Priority::Debug => Some("debug"),
        }
    }

    /// Plain-text log files have no structured priority field, so this is
    /// a best-effort keyword match: this level's usual spelling plus every
    /// more severe one. `All`/`Info`/`Notice`/`Debug` don't filter — at
    /// that point nearly every line would match anyway.
    fn file_grep_pattern(&self) -> Option<&'static str> {
        match self {
            Priority::All | Priority::Debug | Priority::Info | Priority::Notice => None,
            Priority::Warning => Some(r"\b(warn|warning|error|err|fatal|crit|critical|alert|emerg|emergency)\b"),
            Priority::Err => Some(r"\b(error|err|fatal|crit|critical|alert|emerg|emergency)\b"),
            Priority::Crit => Some(r"\b(crit|critical|alert|emerg|emergency)\b"),
            Priority::Alert => Some(r"\b(alert|emerg|emergency)\b"),
            Priority::Emerg => Some(r"\b(emerg|emergency)\b"),
        }
    }
}

pub struct FetchParams {
    pub source: Source,
    /// Journal mode: systemd unit name (e.g. `nginx.service`), empty = all units.
    pub unit: String,
    /// Journal mode: `--since` value (e.g. `1 hour ago`, `-30min`, `2024-01-01`).
    pub since: String,
    /// File mode: path to tail (e.g. `/var/log/syslog`).
    pub path: String,
    pub priority: Priority,
    pub search: String,
    pub lines: u32,
}

fn quote(s: &str) -> String {
    format!("'{}'", escape_single_quotes(s))
}

fn build_command(params: &FetchParams) -> String {
    match params.source {
        Source::Journal => {
            let mut parts = vec!["journalctl".to_string(), "--no-pager".to_string(), "-o".to_string(), "short-iso".to_string(), format!("-n {}", params.lines)];
            if let Some(p) = params.priority.journalctl_arg() {
                parts.push(format!("-p {p}"));
            }
            if !params.unit.trim().is_empty() {
                parts.push(format!("-u {}", quote(params.unit.trim())));
            }
            if !params.since.trim().is_empty() {
                parts.push(format!("--since {}", quote(params.since.trim())));
            }
            if !params.search.trim().is_empty() {
                parts.push(format!("-g {}", quote(params.search.trim())));
            }
            parts.join(" ")
        }
        Source::File => {
            let mut cmd = format!("tail -n {} -- {}", params.lines, quote(&params.path));
            if let Some(re) = params.priority.file_grep_pattern() {
                cmd = format!("{cmd} | grep -iE {}", quote(re));
            }
            if !params.search.trim().is_empty() {
                cmd = format!("{cmd} | grep -i {}", quote(params.search.trim()));
            }
            cmd
        }
    }
}

/// Runs the built command and returns its output split into lines. A
/// nonzero exit with empty stdout/stderr (typical of `grep` finding no
/// matches at the end of a pipeline) is treated as "no matching lines"
/// rather than an error — a real failure always has something on stderr.
pub fn fetch_logs(sess: &SshSession, params: &FetchParams) -> Result<Vec<String>, String> {
    let cmd = build_command(params);
    let (stdout, stderr, code) = sess.exec_raw(&cmd)?;
    if code != 0 {
        if stderr.trim().is_empty() && stdout.trim().is_empty() {
            return Ok(Vec::new());
        }
        if !stderr.trim().is_empty() {
            return Err(stderr);
        }
    }
    Ok(stdout.lines().map(|l| l.to_string()).collect())
}

pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
}

/// Lists `path` (defaulting to `/var/log`) non-recursively. Directories
/// are distinguished via `ls -p`'s trailing `/` instead of parsing `ls -l`
/// column output, which varies across `coreutils`/BusyBox/macOS.
pub fn list_dir(sess: &SshSession, path: &str) -> Result<Vec<RemoteEntry>, String> {
    let p = if path.trim().is_empty() { "/var/log" } else { path.trim() };
    let cmd = format!("ls -1p -- {}", quote(p));
    let (stdout, _stderr) = sess.exec_checked(&cmd)?;

    let mut entries: Vec<RemoteEntry> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let is_dir = l.ends_with('/');
            let name = l.trim_end_matches('/').to_string();
            RemoteEntry { name, is_dir }
        })
        .collect();
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(entries)
}

/// Normalizes `..`-joins etc. with plain string ops (no `PathBuf`, since
/// this path lives on the remote host, not the local filesystem).
pub fn join_remote_path(dir: &str, name: &str) -> String {
    if name == ".." {
        let trimmed = dir.trim_end_matches('/');
        match trimmed.rfind('/') {
            Some(0) => "/".to_string(),
            Some(idx) => trimmed[..idx].to_string(),
            None => "/".to_string(),
        }
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}
