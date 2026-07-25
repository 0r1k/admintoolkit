//! Server list backed directly by `~/.ssh/config`, the same file `ssh`
//! itself reads. Tags/pin/last-seen/SSH-count aren't SSH concepts, so they
//! live in a side JSON file (`easyssh.json`, in atk's shared config dir)
//! instead of being smuggled into the SSH config as comments.
//!
//! Field coverage: the dozen or so options people actually set by hand
//! (HostName/User/Port/IdentityFile/ProxyJump/forwarding/ForwardAgent/
//! StrictHostKeyChecking/ConnectTimeout) get dedicated fields; every other
//! directive round-trips through `Server::extra` so nothing is ever lost,
//! even though it doesn't get its own widget.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::ssh_config_parser::{HostBlock, SshConfigFile};
use crate::config::config_file;

#[derive(Debug, Clone, Default)]
pub struct Server {
    pub alias: String,
    /// Every `Host` pattern on the header line, including wildcards, in
    /// original order — preserved so editing a block with extra patterns
    /// doesn't silently drop them. `alias` is always `patterns[0]`.
    pub patterns: Vec<String>,
    pub host: String,
    pub user: String,
    /// `0` means unset (SSH's own default of 22 applies).
    pub port: u16,
    pub identity_files: Vec<String>,
    pub proxy_jump: String,
    /// CLI form (`[bind:]port:host:hostport`), the same syntax `ssh -L` takes.
    pub local_forward: Vec<String>,
    pub remote_forward: Vec<String>,
    pub dynamic_forward: Vec<String>,
    pub forward_agent: String,
    pub strict_host_key_checking: String,
    pub connect_timeout: String,
    /// Every other directive this block had, verbatim key/value pairs —
    /// the "Advanced" escape hatch.
    pub extra: Vec<(String, String)>,

    // Metadata (easyssh.json), not part of ~/.ssh/config.
    pub tags: Vec<String>,
    pub last_seen: Option<String>,
    pub pinned_at: Option<String>,
    pub ssh_count: u32,
    /// Optional password for this host, stored encrypted in `easyssh.json`
    /// (never in `~/.ssh/config`, which has no such field). This is what
    /// makes a saved host a reusable "profile": every SSH-backed tool that
    /// already pulls host/port/user/key from here via the host picker also
    /// gets the password, instead of it having to be retyped per tool.
    pub ssh_password: String,
}

impl Server {
    pub fn is_pinned(&self) -> bool {
        self.pinned_at.is_some()
    }

    /// The host to actually connect to — `HostName` if the block set one,
    /// otherwise `alias` itself, exactly like `ssh` behaves when a `Host`
    /// block has no `HostName` (the pattern doubles as the target, which
    /// is the common case for a block literally named after an IP). Every
    /// consumer that shows or copies "the host" from a `Server` should go
    /// through this rather than reading `.host` directly, or a bare-IP
    /// alias with no `HostName` renders/fills in as blank.
    pub fn effective_host(&self) -> &str {
        if self.host.is_empty() {
            &self.alias
        } else {
            &self.host
        }
    }
}

const CORE_KEYS: &[&str] = &[
    "hostname",
    "user",
    "port",
    "identityfile",
    "proxyjump",
    "localforward",
    "remoteforward",
    "dynamicforward",
    "forwardagent",
    "stricthostkeychecking",
    "connecttimeout",
];

fn host_block_to_server(block: &HostBlock) -> Option<Server> {
    let alias = block.primary_alias()?;
    let mut server = Server {
        alias,
        patterns: block.patterns.clone(),
        port: block.get_first("Port").and_then(|p| p.parse().ok()).unwrap_or(0),
        ..Default::default()
    };
    server.host = block.get_first("HostName").unwrap_or("").to_string();
    server.user = block.get_first("User").unwrap_or("").to_string();
    server.identity_files = block.get_all("IdentityFile");
    server.proxy_jump = block.get_first("ProxyJump").unwrap_or("").to_string();
    server.local_forward = block.get_all("LocalForward").iter().map(|f| config_to_cli_forward(f)).collect();
    server.remote_forward = block.get_all("RemoteForward").iter().map(|f| config_to_cli_forward(f)).collect();
    server.dynamic_forward = block.get_all("DynamicForward");
    server.forward_agent = block.get_first("ForwardAgent").unwrap_or("").to_string();
    server.strict_host_key_checking = block.get_first("StrictHostKeyChecking").unwrap_or("").to_string();
    server.connect_timeout = block.get_first("ConnectTimeout").unwrap_or("").to_string();
    server.extra = block.directives.iter().filter(|(k, _)| !CORE_KEYS.contains(&k.to_lowercase().as_str())).cloned().collect();
    Some(server)
}

fn server_to_directives(server: &Server) -> Vec<(String, String)> {
    let mut d = Vec::new();
    if !server.host.is_empty() {
        d.push(("HostName".to_string(), server.host.clone()));
    }
    if !server.user.is_empty() {
        d.push(("User".to_string(), server.user.clone()));
    }
    if server.port != 0 {
        d.push(("Port".to_string(), server.port.to_string()));
    }
    for f in &server.identity_files {
        if !f.is_empty() {
            d.push(("IdentityFile".to_string(), f.clone()));
        }
    }
    if !server.proxy_jump.is_empty() {
        d.push(("ProxyJump".to_string(), server.proxy_jump.clone()));
    }
    for f in &server.local_forward {
        if !f.is_empty() {
            d.push(("LocalForward".to_string(), cli_to_config_forward(f)));
        }
    }
    for f in &server.remote_forward {
        if !f.is_empty() {
            d.push(("RemoteForward".to_string(), cli_to_config_forward(f)));
        }
    }
    for f in &server.dynamic_forward {
        if !f.is_empty() {
            d.push(("DynamicForward".to_string(), f.clone()));
        }
    }
    if !server.forward_agent.is_empty() {
        d.push(("ForwardAgent".to_string(), server.forward_agent.clone()));
    }
    if !server.strict_host_key_checking.is_empty() {
        d.push(("StrictHostKeyChecking".to_string(), server.strict_host_key_checking.clone()));
    }
    if !server.connect_timeout.is_empty() {
        d.push(("ConnectTimeout".to_string(), server.connect_timeout.clone()));
    }
    for (k, v) in &server.extra {
        if !k.is_empty() && !v.is_empty() {
            d.push((k.clone(), v.clone()));
        }
    }
    d
}

/// ssh_config format: `[bind_address:]port host:hostport` -> CLI format:
/// `[bind_address:]port:host:hostport` (what `ssh -L`/`-R` themselves take).
fn config_to_cli_forward(s: &str) -> String {
    match s.rsplit_once(' ') {
        Some((local, remote)) => format!("{local}:{remote}"),
        None => s.to_string(),
    }
}

/// Inverse of `config_to_cli_forward`. Does a plain last-two-segments split
/// rather than being IPv6-bracket-aware, on the assumption that `[::1]`-style
/// forwarding targets are rare enough not to justify the extra parsing
/// complexity here.
fn cli_to_config_forward(s: &str) -> String {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() >= 3 {
        let split_at = parts.len() - 2;
        format!("{} {}", parts[..split_at].join(":"), parts[split_at..].join(":"))
    } else {
        s.to_string()
    }
}

fn validate(server: &Server) -> Result<(), String> {
    let alias = server.alias.trim();
    if alias.is_empty() {
        return Err("alias is required".to_string());
    }
    if !alias.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c)) {
        return Err("alias may contain letters, digits, dot, dash, underscore".to_string());
    }
    if server.host.trim().is_empty() {
        return Err("Host/IP is required".to_string());
    }
    if server.host.contains(' ') {
        return Err("host must not contain spaces".to_string());
    }
    Ok(())
}

// ── ~/.ssh/config I/O + backups ─────────────────────────────────────────

const MAX_BACKUPS: usize = 10;
const BACKUP_SUFFIX: &str = "easyssh.backup";
const ORIGINAL_BACKUP_NAME: &str = "config.original.backup";

pub fn ssh_config_path() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".ssh").join("config")
}

fn read_config_text() -> String {
    std::fs::read_to_string(ssh_config_path()).unwrap_or_default()
}

/// Writes `cfg` back to `~/.ssh/config`: one-time original backup, a
/// timestamped rolling backup (max `MAX_BACKUPS`, oldest pruned), then an
/// atomic temp-file-plus-rename so a crash mid-write can't corrupt the file
/// ssh itself depends on.
fn save_config(cfg: &SshConfigFile) -> Result<(), String> {
    let path = ssh_config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }

    let content = cfg.render();
    let tmp_path = path.with_extension("tmp-atk");
    std::fs::write(&tmp_path, &content).map_err(|e| e.to_string())?;

    if path.exists() {
        create_original_backup_if_needed(&path)?;
        create_rolling_backup(&path)?;
    }

    std::fs::rename(&tmp_path, &path).map_err(|e| e.to_string())?;
    Ok(())
}

fn create_original_backup_if_needed(path: &std::path::Path) -> Result<(), String> {
    let original = path.with_file_name(ORIGINAL_BACKUP_NAME);
    if original.exists() {
        return Ok(());
    }
    std::fs::copy(path, &original).map_err(|e| e.to_string())?;
    Ok(())
}

fn create_rolling_backup(path: &std::path::Path) -> Result<(), String> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    let backup_name = format!("{}-{millis}-{BACKUP_SUFFIX}", path.file_name().and_then(|n| n.to_str()).unwrap_or("config"));
    let backup_path = path.with_file_name(backup_name);
    std::fs::copy(path, &backup_path).map_err(|e| e.to_string())?;

    let dir = path.parent().ok_or("config path has no parent directory")?;
    let mut backups: Vec<(std::fs::DirEntry, std::time::SystemTime)> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(BACKUP_SUFFIX))
        .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (e, t)))
        .collect();
    backups.sort_by(|a, b| b.1.cmp(&a.1));
    for (entry, _) in backups.into_iter().skip(MAX_BACKUPS) {
        let _ = std::fs::remove_file(entry.path());
    }
    Ok(())
}

// ── Metadata (tags / pin / last-seen / ssh-count) ───────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ServerMeta {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pinned_at: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    ssh_count: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    ssh_password_encrypted: String,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn metadata_path() -> PathBuf {
    config_file("easyssh.json")
}

/// One-time migration from this tool's old name (`lazyssh.json`) — moves
/// it to `easyssh.json` in place so tags/pins/history from before the
/// rename aren't orphaned. No-ops once the new file exists.
fn migrate_legacy_metadata_if_needed() {
    let new_path = metadata_path();
    if new_path.exists() {
        return;
    }
    let legacy_path = config_file("lazyssh.json");
    if legacy_path.exists() {
        let _ = std::fs::rename(&legacy_path, &new_path);
    }
}

fn load_metadata() -> HashMap<String, ServerMeta> {
    migrate_legacy_metadata_if_needed();
    let path = metadata_path();
    let Ok(data) = std::fs::read_to_string(&path) else { return HashMap::new() };
    serde_json::from_str(&data).unwrap_or_default()
}

fn save_metadata(meta: &HashMap<String, ServerMeta>) -> Result<(), String> {
    let json = serde_json::to_string_pretty(meta).map_err(|e| e.to_string())?;
    std::fs::write(metadata_path(), json).map_err(|e| e.to_string())
}

fn now_rfc3339() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    // Minimal RFC3339 (UTC, second precision) without pulling in a date
    // crate — good enough since we only ever re-parse what we wrote.
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Days-since-epoch to (year, month, day), civil calendar. Howard Hinnant's
/// well-known constant-time algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Inverse of `civil_from_days`: (year, month, day) -> days-since-epoch.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let mp = ((m as i64 + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn parse_rfc3339(s: &str) -> Option<u64> {
    if s.len() != 20 {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    let h: u64 = s.get(11..13)?.parse().ok()?;
    let mi: u64 = s.get(14..16)?.parse().ok()?;
    let se: u64 = s.get(17..19)?.parse().ok()?;
    let days = days_from_civil(y, mo, d);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86400 + h * 3600 + mi * 60 + se)
}

/// "3h ago" / "never" style relative-time label, for the server list.
pub fn humanize_timestamp(ts: &Option<String>) -> String {
    let Some(ts) = ts else { return "never".to_string() };
    let Some(then) = parse_rfc3339(ts) else { return "never".to_string() };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let diff = now.saturating_sub(then);
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 172_800 {
        format!("{}h ago", diff / 3600)
    } else if diff < 60 * 86400 {
        format!("{}d ago", diff / 86400)
    } else {
        format!("{}mo ago", diff / (30 * 86400))
    }
}

fn apply_metadata(servers: &mut [Server], meta: &HashMap<String, ServerMeta>) {
    for s in servers.iter_mut() {
        if let Some(m) = meta.get(&s.alias) {
            s.tags = m.tags.clone();
            s.last_seen = m.last_seen.clone();
            s.pinned_at = m.pinned_at.clone();
            s.ssh_count = m.ssh_count;
            s.ssh_password = crate::secret::decrypt_optional(&m.ssh_password_encrypted).unwrap_or_default();
        }
    }
}

fn matches_query(s: &Server, q: &str) -> bool {
    if s.alias.to_lowercase().contains(q) || s.effective_host().to_lowercase().contains(q) || s.user.to_lowercase().contains(q) {
        return true;
    }
    s.tags.iter().any(|t| t.to_lowercase().contains(q))
}

// ── Public API ───────────────────────────────────────────────────────────

pub fn list_servers(query: &str) -> Result<Vec<Server>, String> {
    let cfg = SshConfigFile::parse(&read_config_text());
    let meta = load_metadata();
    let mut servers: Vec<Server> = cfg.hosts.iter().filter_map(host_block_to_server).collect();
    apply_metadata(&mut servers, &meta);

    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Ok(servers);
    }
    Ok(servers.into_iter().filter(|s| matches_query(s, &q)).collect())
}

pub fn add_server(server: &Server) -> Result<(), String> {
    validate(server)?;
    let mut cfg = SshConfigFile::parse(&read_config_text());
    if cfg.alias_exists(&server.alias) {
        return Err(format!("server with alias '{}' already exists", server.alias));
    }
    cfg.hosts.push(HostBlock::new(vec![server.alias.clone()], server_to_directives(server)));
    save_config(&cfg)?;

    let mut meta = load_metadata();
    let ssh_password_encrypted = crate::secret::encrypt_optional(&server.ssh_password).map_err(|e| e.to_string())?;
    meta.insert(server.alias.clone(), ServerMeta { tags: server.tags.clone(), ssh_password_encrypted, ..Default::default() });
    save_metadata(&meta)
}

pub fn update_server(old_alias: &str, server: &Server) -> Result<(), String> {
    validate(server)?;
    let mut cfg = SshConfigFile::parse(&read_config_text());
    let idx = cfg.find_index(old_alias).ok_or_else(|| format!("server with alias '{old_alias}' not found"))?;
    if old_alias != server.alias && cfg.alias_exists(&server.alias) {
        return Err(format!("server with alias '{}' already exists", server.alias));
    }
    cfg.hosts[idx] = HostBlock::new(vec![server.alias.clone()], server_to_directives(server));
    save_config(&cfg)?;

    let mut meta = load_metadata();
    let mut entry = if old_alias != server.alias { meta.remove(old_alias).unwrap_or_default() } else { meta.remove(&server.alias).unwrap_or_default() };
    entry.tags = server.tags.clone();
    // Leaving the password field empty on an edit keeps whatever was
    // already stored, the same convention the DB managers use — otherwise
    // opening "Edit" and saving without retyping the password would wipe it.
    if !server.ssh_password.is_empty() {
        entry.ssh_password_encrypted = crate::secret::encrypt_optional(&server.ssh_password).map_err(|e| e.to_string())?;
    }
    meta.insert(server.alias.clone(), entry);
    save_metadata(&meta)
}

pub fn delete_server(alias: &str) -> Result<(), String> {
    let mut cfg = SshConfigFile::parse(&read_config_text());
    let idx = cfg.find_index(alias).ok_or_else(|| format!("server with alias '{alias}' not found"))?;
    cfg.hosts.remove(idx);
    save_config(&cfg)?;

    let mut meta = load_metadata();
    meta.remove(alias);
    save_metadata(&meta)
}

/// Tags live only in metadata — no need to touch (and risk reformatting)
/// `~/.ssh/config` just to relabel a server.
pub fn set_tags(alias: &str, tags: Vec<String>) -> Result<(), String> {
    let mut meta = load_metadata();
    let entry = meta.entry(alias.to_string()).or_default();
    entry.tags = tags;
    save_metadata(&meta)
}

pub fn set_pinned(alias: &str, pinned: bool) -> Result<(), String> {
    let mut meta = load_metadata();
    let entry = meta.entry(alias.to_string()).or_default();
    entry.pinned_at = if pinned { Some(now_rfc3339()) } else { None };
    save_metadata(&meta)
}

pub fn record_ssh(alias: &str) -> Result<(), String> {
    let mut meta = load_metadata();
    let entry = meta.entry(alias.to_string()).or_default();
    entry.last_seen = Some(now_rfc3339());
    entry.ssh_count += 1;
    save_metadata(&meta)
}
