//! ClickHouse user management over SSH: writes/reads per-user XML config
//! files under `/etc/clickhouse-server/users.d/`. Ported from the original
//! Go implementation (golang.org/x/crypto/ssh) onto the shared ssh_exec.

use std::cmp::Ordering;
use std::path::Path;

use rand::Rng;
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::ssh_exec::{escape_single_quotes, SshSession};

use super::config;

#[derive(Debug, Clone)]
pub struct UserInfo {
    pub name: String,
    pub profile: String,
    pub ips: Vec<String>,
}

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn generate_password() -> (String, String) {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    let password: String = (0..25)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect();
    let hash = sha256_hex(&password);
    (password, hash)
}

fn generate_password_hash(password: &str) -> (String, String) {
    if password.is_empty() {
        return generate_password();
    }
    (password.to_string(), sha256_hex(password))
}

fn build_user_xml(root_tag: &str, username: &str, password_hash: &str, profile: &str, ips: &[String]) -> String {
    let mut all_ips = vec!["127.0.0.1".to_string()];
    all_ips.extend(ips.iter().cloned());
    let ip_xml: String = all_ips
        .iter()
        .map(|ip| format!("            <ip>{ip}</ip>\n"))
        .collect();

    format!(
        "<{root}>\n    <users>\n        <{u}>\n            <password_sha256_hex>{hash}</password_sha256_hex>\n            <networks incl=\"networks\" replace=\"replace\">\n{ips}            </networks>\n            <profile>{profile}</profile>\n            <quota>default</quota>\n        </{u}>\n    </users>\n</{root}>",
        root = root_tag,
        u = username,
        hash = password_hash,
        ips = ip_xml,
        profile = profile,
    )
}

fn compare_semver(left: &str, right: &str) -> Ordering {
    fn parse(v: &str) -> [i64; 3] {
        let mut out = [0i64; 3];
        for (i, part) in v.split('.').take(3).enumerate() {
            out[i] = part.parse().unwrap_or(0);
        }
        out
    }
    parse(left).cmp(&parse(right))
}

fn extract_version(raw: &str) -> Result<String, String> {
    let re = Regex::new(r"\d+(?:\.\d+){0,2}").unwrap();
    re.find(raw)
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| format!("unable to parse version from output: {}", raw.trim()))
}

fn detect_clickhouse_version(sess: &SshSession) -> Result<String, String> {
    const CMDS: &[&str] = &[
        "clickhouse-client --query \"SELECT version()\" 2>/dev/null",
        "clickhouse server --version 2>/dev/null",
        "clickhouse-server --version 2>/dev/null",
    ];
    let mut last_err = None;
    for cmd in CMDS {
        match sess.exec_checked(cmd) {
            Ok((out, _)) => match extract_version(&out) {
                Ok(v) => return Ok(v),
                Err(e) => last_err = Some(e),
            },
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| "unable to detect clickhouse version".to_string()))
}

/// What root tag `/etc/clickhouse-server/config.xml` (the *main* server
/// config, not the fragment we're about to write) actually uses, if it's
/// readable. This is the authoritative answer for "auto" mode — ClickHouse
/// requires every fragment under `users.d/`/`config.d/` to use the same
/// root tag as the main config, regardless of server version. A server
/// upgraded in place past 21.8 (when `<clickhouse>` became a valid alias)
/// but whose `config.xml` was never regenerated still only accepts
/// `<yandex>` fragments and rejects `<clickhouse>` ones with a
/// `POCO_EXCEPTION`, so version-sniffing alone can guess wrong.
fn detect_root_tag_from_main_config(sess: &SshSession) -> Option<String> {
    let cmd = "sudo -i bash -c 'grep -m1 -oE \"<(yandex|clickhouse)>\" /etc/clickhouse-server/config.xml 2>/dev/null || true'";
    let (out, _) = sess.exec_checked(cmd).ok()?;
    let out = out.trim();
    if out.contains("<yandex>") {
        Some("yandex".to_string())
    } else if out.contains("<clickhouse>") {
        Some("clickhouse".to_string())
    } else {
        None
    }
}

fn resolve_root_tag(sess: &SshSession, mode: &str) -> Result<String, String> {
    match mode {
        config::TAG_CLICKHOUSE => Ok("clickhouse".to_string()),
        config::TAG_YANDEX => Ok("yandex".to_string()),
        "" | config::TAG_AUTO => {
            if let Some(tag) = detect_root_tag_from_main_config(sess) {
                return Ok(tag);
            }
            // config.xml wasn't readable or didn't match either tag —
            // fall back to the version heuristic (21.8+ supports
            // <clickhouse> as the root tag for user config files).
            const CLICKHOUSE_ROOT_FROM_VERSION: &str = "21.8.0";
            let version = detect_clickhouse_version(sess)
                .map_err(|e| format!("failed to detect clickhouse version for auto mode: {e}"))?;
            if compare_semver(&version, CLICKHOUSE_ROOT_FROM_VERSION) != Ordering::Less {
                Ok("clickhouse".to_string())
            } else {
                Ok("yandex".to_string())
            }
        }
        other => Err(format!("unsupported tag mode: {other}")),
    }
}

fn file_exists(sess: &SshSession, path: &str) -> Result<bool, String> {
    let cmd = format!("sudo -i bash -c 'test -f {path} && echo exists || echo not_exists'");
    let (result, _) = sess
        .exec_checked(&cmd)
        .map_err(|e| format!("failed to check user existence: {e}"))?;
    Ok(result.trim() == "exists")
}

/// Creates `/etc/clickhouse-server/users.d/<username>.xml`. Returns the
/// plaintext password (generated if `password` was empty).
pub fn create_user(
    sess: &SshSession,
    username: &str,
    profile: &str,
    ips: &[String],
    tag_mode: &str,
    password: &str,
) -> Result<String, String> {
    let path = format!("/etc/clickhouse-server/users.d/{username}.xml");
    if file_exists(sess, &path)? {
        return Err(format!("user {username} already exists"));
    }

    let (password, password_hash) = generate_password_hash(password);
    let root_tag = resolve_root_tag(sess, tag_mode)?;
    let xml_content = build_user_xml(&root_tag, username, &password_hash, profile, ips);

    let create_cmd = format!(
        "echo '{}' | sudo tee {path} > /dev/null && sudo chmod 644 {path} && sudo chown clickhouse:clickhouse {path}",
        escape_single_quotes(&xml_content)
    );
    sess.exec_checked(&create_cmd)
        .map_err(|e| format!("failed to create user config: {e}"))?;

    Ok(password)
}

pub fn list_users(sess: &SshSession) -> Result<Vec<UserInfo>, String> {
    let cmd = "sudo -i bash -c 'ls /etc/clickhouse-server/users.d/*.xml 2>/dev/null || true'";
    let (result, _) = sess
        .exec_checked(cmd)
        .map_err(|e| format!("failed to list users: {e}"))?;

    let mut users = Vec::new();
    for file in result.trim().split('\n') {
        let file = file.trim();
        if file.is_empty() {
            continue;
        }
        let basename = Path::new(file)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(file);
        if basename == "default.xml" || basename == "readonly.xml" {
            continue;
        }
        let username = basename.trim_end_matches(".xml");

        let cat_cmd = format!("sudo -i cat {file}");
        let content = match sess.exec_checked(&cat_cmd) {
            Ok((out, _)) => out,
            Err(_) => continue,
        };

        users.push(parse_user_xml(username, &content));
    }
    Ok(users)
}

fn parse_user_xml(username: &str, content: &str) -> UserInfo {
    let mut info = UserInfo {
        name: username.to_string(),
        profile: String::new(),
        ips: Vec::new(),
    };

    if let Some(idx) = content.find("<profile>") {
        if let Some(end_idx) = content[idx..].find("</profile>") {
            info.profile = content[idx + 9..idx + end_idx].trim().to_string();
        }
    }

    for line in content.lines() {
        let line = line.trim();
        if let Some(ip) = line.strip_prefix("<ip>").and_then(|s| s.strip_suffix("</ip>")) {
            if ip != "127.0.0.1" && ip != "::1" {
                info.ips.push(ip.to_string());
            }
        }
    }

    info
}

fn extract_password_hash(content: &str) -> Option<String> {
    let idx = content.find("<password_sha256_hex>")?;
    let end_idx = content[idx..].find("</password_sha256_hex>")?;
    Some(content[idx + 22..idx + end_idx].trim().to_string())
}

/// Rewrites an existing user's profile/allowed-IPs, and its password if
/// `new_password` is non-empty (otherwise the existing hash is preserved
/// by reading it back out of the current file first).
pub fn update_user(
    sess: &SshSession,
    username: &str,
    profile: &str,
    ips: &[String],
    tag_mode: &str,
    new_password: &str,
) -> Result<(), String> {
    let path = format!("/etc/clickhouse-server/users.d/{username}.xml");
    if !file_exists(sess, &path)? {
        return Err(format!("user {username} does not exist"));
    }

    let password_hash = if new_password.is_empty() {
        let cat_cmd = format!("sudo -i cat {path}");
        let (content, _) = sess.exec_checked(&cat_cmd).map_err(|e| format!("failed to read existing user config: {e}"))?;
        extract_password_hash(&content).ok_or_else(|| "could not read existing password hash".to_string())?
    } else {
        sha256_hex(new_password)
    };

    let root_tag = resolve_root_tag(sess, tag_mode)?;
    let xml_content = build_user_xml(&root_tag, username, &password_hash, profile, ips);
    let write_cmd = format!(
        "echo '{}' | sudo tee {path} > /dev/null && sudo chmod 644 {path} && sudo chown clickhouse:clickhouse {path}",
        escape_single_quotes(&xml_content)
    );
    sess.exec_checked(&write_cmd).map_err(|e| format!("failed to update user config: {e}"))?;
    Ok(())
}

pub fn delete_user(sess: &SshSession, username: &str) -> Result<(), String> {
    let path = format!("/etc/clickhouse-server/users.d/{username}.xml");
    if !file_exists(sess, &path)? {
        return Err(format!("user {username} does not exist"));
    }

    let delete_cmd = format!("sudo -i rm -f {path}");
    sess.exec_checked(&delete_cmd)
        .map_err(|e| format!("failed to delete user: {e}"))?;

    Ok(())
}
