//! Direct ClickHouse user management over its HTTP interface — issues
//! plain `CREATE USER`/`ALTER USER`/`DROP USER` SQL instead of hand-editing
//! `users.d/*.xml` over SSH (see `super::client` for that route). Reaches
//! the HTTP port either directly or through an SSH tunnel (see
//! `crate::ssh_tunnel`), exactly like the MySQL/PostgreSQL managers.

use std::time::Duration;

use reqwest::blocking::Client as HttpClient;
use serde::Deserialize;

use super::client::UserInfo;
use super::config::ConnectionWithSecrets;
use crate::ssh_exec::Credentials;

/// Opens a tunnel first when the profile asks for one, then returns the
/// host/port an HTTP client should actually hit. The tunnel's forwarding
/// thread runs independently once started (see `crate::ssh_tunnel::open`),
/// so nothing further needs to be kept alive here.
fn resolve_target(cfg: &ConnectionWithSecrets) -> Result<(String, u16), String> {
    let port: u16 = cfg.port.trim().parse().map_err(|_| format!("invalid port {:?}", cfg.port))?;
    if cfg.use_tunnel {
        let creds = Credentials {
            user: cfg.ssh_user.clone(),
            password: cfg.ssh_password.clone(),
            private_key_path: cfg.ssh_key_path.clone(),
        };
        let tunnel = crate::ssh_tunnel::open(&cfg.ssh_host, &cfg.ssh_port, &creds, &cfg.host, port)
            .map_err(|e| format!("SSH tunnel failed: {e}"))?;
        Ok(("127.0.0.1".to_string(), tunnel.local_port))
    } else {
        Ok((cfg.host.clone(), port))
    }
}

fn execute(cfg: &ConnectionWithSecrets, sql: &str) -> Result<String, String> {
    let (host, port) = resolve_target(cfg)?;
    let url = format!("http://{host}:{port}/");
    let client = HttpClient::builder().timeout(Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
    let resp = client
        .post(&url)
        .basic_auth(&cfg.db_user, Some(&cfg.db_password))
        .body(sql.to_string())
        .send()
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("ClickHouse HTTP error {status}: {}", body.trim()));
    }
    Ok(body)
}

#[derive(Deserialize)]
struct UserRow {
    name: String,
    #[serde(default)]
    host_ip: Vec<String>,
}

#[derive(Deserialize)]
struct ProfileRow {
    user_name: Option<String>,
    profile_name: Option<String>,
}

pub fn list_users(cfg: &ConnectionWithSecrets) -> Result<Vec<UserInfo>, String> {
    let body = execute(cfg, "SELECT name, host_ip FROM system.users FORMAT JSONEachRow")?;
    let mut users: Vec<UserInfo> = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| serde_json::from_str::<UserRow>(line).map_err(|e| format!("failed to parse system.users row: {e}")))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|r| r.name != "default")
        .map(|r| UserInfo {
            name: r.name,
            profile: String::new(),
            ips: r.host_ip.into_iter().filter(|ip| ip != "127.0.0.1" && ip != "::1").collect(),
        })
        .collect();

    // Best-effort profile enrichment — a user created outside our own
    // `SETTINGS PROFILE` convention (e.g. via a role) just shows blank.
    if !users.is_empty() {
        if let Ok(prof_body) = execute(
            cfg,
            "SELECT user_name, profile_name FROM system.settings_profile_elements \
             WHERE user_name IS NOT NULL AND profile_name IS NOT NULL FORMAT JSONEachRow",
        ) {
            for line in prof_body.lines().filter(|l| !l.trim().is_empty()) {
                if let Ok(row) = serde_json::from_str::<ProfileRow>(line) {
                    if let (Some(uname), Some(pname)) = (row.user_name, row.profile_name) {
                        if let Some(u) = users.iter_mut().find(|u| u.name == uname) {
                            u.profile = pname;
                        }
                    }
                }
            }
        }
    }

    Ok(users)
}

fn host_clause(ips: &[String]) -> String {
    let mut all_ips = vec!["127.0.0.1".to_string()];
    all_ips.extend(ips.iter().cloned());
    all_ips.iter().map(|ip| format!("IP {}", quote_lit(ip))).collect::<Vec<_>>().join(" ")
}

/// Creates a user and returns the plaintext password (generated if
/// `password` was empty) — ClickHouse hashes it server-side.
pub fn create_user(cfg: &ConnectionWithSecrets, username: &str, profile: &str, ips: &[String], password: &str) -> Result<String, String> {
    let (password, _hash) = if password.is_empty() { super::client::generate_password() } else { (password.to_string(), String::new()) };
    let sql = format!(
        "CREATE USER {} IDENTIFIED WITH sha256_password BY {} HOST {} SETTINGS PROFILE {}",
        quote_ident(username),
        quote_lit(&password),
        host_clause(ips),
        quote_lit(profile),
    );
    execute(cfg, &sql)?;
    Ok(password)
}

/// Updates an existing user's profile/allowed-IPs, and its password if
/// `new_password` is non-empty (blank leaves the current password as-is).
pub fn update_user(cfg: &ConnectionWithSecrets, username: &str, profile: &str, ips: &[String], new_password: &str) -> Result<(), String> {
    let mut clauses = Vec::new();
    if !new_password.is_empty() {
        clauses.push(format!("IDENTIFIED WITH sha256_password BY {}", quote_lit(new_password)));
    }
    clauses.push(format!("HOST {}", host_clause(ips)));
    clauses.push(format!("SETTINGS PROFILE {}", quote_lit(profile)));
    let sql = format!("ALTER USER {} {}", quote_ident(username), clauses.join(" "));
    execute(cfg, &sql)?;
    Ok(())
}

pub fn delete_user(cfg: &ConnectionWithSecrets, username: &str) -> Result<(), String> {
    execute(cfg, &format!("DROP USER {}", quote_ident(username)))?;
    Ok(())
}

/// Escapes a ClickHouse string literal.
fn quote_lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Escapes a ClickHouse identifier (user name).
fn quote_ident(s: &str) -> String {
    format!("`{}`", s.replace('`', "\\`"))
}
