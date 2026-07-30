//! Read-only discovery: what's listening on port 443, which web server
//! that is (kind, version, binary), every HTTPS vhost it's configured
//! with (domains + cert/key/CA file paths, parsed straight out of the
//! live nginx/apache config), and each cert's expiry. Nothing in this
//! file writes anything — see `engine.rs` for the update side.

use regex::Regex;

use super::exec::{shell_quote, ExecSession};

#[derive(Debug, Clone, Default)]
pub struct WebServer {
    pub kind: String, // "nginx", "apache", or "unknown"
    pub version: String,
    pub binary: String,
    pub pid: String,
}

#[derive(Debug, Clone)]
pub struct VHost {
    pub domains: Vec<String>,
    pub cert_file: String,
    pub key_file: String,
    /// `ssl_trusted_certificate` (nginx) / `SSLCertificateChainFile`
    /// (apache) — set only when the CA/chain lives in its own file
    /// separate from `cert_file`. `None` means either there's no CA
    /// configured at all, or (far more commonly) it's bundled directly
    /// into `cert_file` alongside the leaf cert.
    pub ca_file: Option<String>,
    pub config_file: String,
    pub not_after: String,
    pub days_left: Option<i64>,
    pub cert_error: Option<String>,
}

pub struct DetectResult {
    pub server: WebServer,
    pub vhosts: Vec<VHost>,
}

pub fn detect(session: &ExecSession) -> Result<DetectResult, String> {
    let (pid, proc_name) = port_holder(session)?;
    let server = identify_server(session, &pid, &proc_name);
    let vhosts = match server.kind.as_str() {
        "nginx" => detect_nginx(session, &server.binary)?,
        "apache" => detect_apache(session, &server.binary)?,
        _ => Vec::new(),
    };
    let vhosts = fill_cert_info(session, vhosts);
    Ok(DetectResult { server, vhosts })
}

/// Finds the (pid, process-name) of whatever's holding the listening
/// socket on :443. Requires root to see another user's socket owner, so
/// this always runs under `sudo -n` — on a box with no NOPASSWD rule for
/// this user, that surfaces as a clear permission error rather than a
/// silent "nothing found".
fn port_holder(session: &ExecSession) -> Result<(String, String), String> {
    let cmd = super::exec::sudo("ss -ltnp 'sport = :443' 2>&1");
    let (out, _, _) = session.run(&cmd)?;
    let re = Regex::new(r#"users:\(\("([^"]+)",pid=(\d+)"#).unwrap();
    for line in out.lines() {
        if !line.contains(":443") {
            continue;
        }
        if let Some(caps) = re.captures(line) {
            return Ok((caps[2].to_string(), caps[1].to_string()));
        }
    }
    if out.to_lowercase().contains("permission denied") || out.to_lowercase().contains("password") {
        return Err("could not inspect port 443 (sudo without a password is required for this user)".to_string());
    }
    Err("nothing is listening on port 443 on this host".to_string())
}

fn identify_server(session: &ExecSession, pid: &str, proc_name: &str) -> WebServer {
    let binary = session
        .run(&super::exec::sudo(&format!("readlink -f /proc/{pid}/exe 2>/dev/null")))
        .ok()
        .map(|(out, _, _)| out.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| proc_name.to_string());

    let lower = binary.to_lowercase();
    let kind = if lower.contains("nginx") {
        "nginx"
    } else if lower.contains("apache2") || lower.contains("httpd") {
        "apache"
    } else {
        "unknown"
    };

    let version = match kind {
        "nginx" => session
            .run(&format!("{} -v 2>&1", shell_quote(&binary)))
            .ok()
            .and_then(|(out, err, _)| Regex::new(r"nginx/([\d.]+)").unwrap().captures(&format!("{out} {err}")).map(|c| c[1].to_string()))
            .unwrap_or_default(),
        "apache" => session
            .run(&format!("{} -v 2>&1", shell_quote(&binary)))
            .ok()
            .and_then(|(out, err, _)| Regex::new(r"Apache/([\d.]+)").unwrap().captures(&format!("{out} {err}")).map(|c| c[1].to_string()))
            .unwrap_or_default(),
        _ => session
            .run(&format!("{} --version 2>&1 | head -n1", shell_quote(&binary)))
            .map(|(out, _, _)| out.trim().to_string())
            .unwrap_or_default(),
    };

    WebServer { kind: kind.to_string(), version, binary, pid: pid.to_string() }
}

// ── nginx ────────────────────────────────────────────────────────────────

/// Runs `nginx -T` (the effective, fully-`include`-resolved config) and
/// pulls out every `server { }` block that answers on :443, with its
/// `server_name`, `ssl_certificate`, `ssl_certificate_key` and (if
/// present) `ssl_trusted_certificate`.
fn detect_nginx(session: &ExecSession, binary: &str) -> Result<Vec<VHost>, String> {
    let (out, err) = session.run_checked(&super::exec::sudo(&format!("{} -T 2>&1", shell_quote(binary))))?;
    let text = if out.trim().is_empty() { err } else { out };
    Ok(parse_nginx_config(&text))
}

#[derive(Default)]
struct NginxBlock {
    listen_443: bool,
    domains: Vec<String>,
    cert_file: String,
    key_file: String,
    ca_file: Option<String>,
}

fn parse_nginx_config(text: &str) -> Vec<VHost> {
    let mut stack: Vec<String> = Vec::new();
    let mut server_blocks: Vec<NginxBlock> = Vec::new();
    let mut cur: Option<NginxBlock> = None;
    let mut word_buf = String::new();

    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        for tok in split_nginx_line(line) {
            match tok.as_str() {
                "{" => {
                    let keyword = word_buf.split_whitespace().next().unwrap_or("").to_string();
                    word_buf.clear();
                    if keyword == "server" && stack.last().map(|s| s != "server").unwrap_or(true) {
                        cur = Some(NginxBlock::default());
                    }
                    stack.push(keyword);
                }
                "}" => {
                    let closed = stack.pop().unwrap_or_default();
                    if closed == "server" && stack.last().map(|s| s != "server").unwrap_or(true) {
                        if let Some(b) = cur.take() {
                            server_blocks.push(b);
                        }
                    }
                }
                ";" => {
                    if let Some(b) = cur.as_mut() {
                        if stack.last().map(|s| s == "server").unwrap_or(false) {
                            apply_nginx_directive(&word_buf, b);
                        }
                    }
                    word_buf.clear();
                }
                other => {
                    if !word_buf.is_empty() {
                        word_buf.push(' ');
                    }
                    word_buf.push_str(other);
                }
            }
        }
    }

    server_blocks
        .into_iter()
        .filter(|b| b.listen_443 && !b.cert_file.is_empty())
        .map(|b| VHost {
            domains: b.domains,
            cert_file: b.cert_file,
            key_file: b.key_file,
            ca_file: b.ca_file,
            config_file: "nginx -T".to_string(),
            not_after: String::new(),
            days_left: None,
            cert_error: None,
        })
        .collect()
}

/// Splits one config line into directive-arg tokens plus standalone `{`
/// `}` `;` tokens, the only punctuation this parser cares about.
fn split_nginx_line(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut buf = String::new();
    for c in line.chars() {
        match c {
            '{' | '}' | ';' => {
                if !buf.trim().is_empty() {
                    tokens.push(buf.trim().to_string());
                }
                buf.clear();
                tokens.push(c.to_string());
            }
            _ => buf.push(c),
        }
    }
    if !buf.trim().is_empty() {
        tokens.push(buf.trim().to_string());
    }
    tokens
}

fn apply_nginx_directive(stmt: &str, b: &mut NginxBlock) {
    let mut parts = stmt.split_whitespace();
    let Some(directive) = parts.next() else { return };
    let args: Vec<&str> = parts.collect();
    match directive {
        "listen" => {
            if args.iter().any(|a| a.contains("443")) {
                b.listen_443 = true;
            }
        }
        "server_name" => {
            for d in args {
                b.domains.push(d.trim_matches('"').trim_matches('\'').to_string());
            }
        }
        "ssl_certificate" => {
            if let Some(v) = args.first() {
                b.cert_file = unquote(v);
            }
        }
        "ssl_certificate_key" => {
            if let Some(v) = args.first() {
                b.key_file = unquote(v);
            }
        }
        "ssl_trusted_certificate" => {
            if let Some(v) = args.first() {
                b.ca_file = Some(unquote(v));
            }
        }
        _ => {}
    }
}

fn unquote(s: &str) -> String {
    s.trim_matches('"').trim_matches('\'').to_string()
}

// ── apache ───────────────────────────────────────────────────────────────

/// `apache2ctl -S` / `httpd -S` / `apachectl -S` prints one line per
/// configured vhost naming the config file (and line number) it came
/// from — used only to discover *which files* matter, so the actual
/// directives get parsed straight out of the real config rather than
/// this summary (which doesn't show cert paths or `ServerAlias`).
fn detect_apache(session: &ExecSession, binary: &str) -> Result<Vec<VHost>, String> {
    let mut summary = String::new();
    for candidate in ["apache2ctl", "httpd", "apachectl", binary] {
        if let Ok((out, _, code)) = session.run(&super::exec::sudo(&format!("{} -S 2>&1", shell_quote(candidate)))) {
            if code == 0 || out.contains("VirtualHost") || out.contains("namevhost") {
                summary = out;
                break;
            }
        }
    }
    if summary.trim().is_empty() {
        return Ok(Vec::new());
    }

    let re = Regex::new(r"\(([^:()]+):\d+\)").unwrap();
    let mut files: Vec<String> = Vec::new();
    for line in summary.lines() {
        if !line.contains("443") {
            continue;
        }
        if let Some(caps) = re.captures(line) {
            let f = caps[1].to_string();
            if !files.contains(&f) {
                files.push(f);
            }
        }
    }
    if files.is_empty() {
        return Ok(Vec::new());
    }

    let cat_cmd = format!("cat {} 2>/dev/null", files.iter().map(|f| shell_quote(f)).collect::<Vec<_>>().join(" "));
    let (content, _, _) = session.run(&super::exec::sudo(&cat_cmd))?;
    Ok(parse_apache_config(&content, &files.join(", ")))
}

fn parse_apache_config(text: &str, config_file: &str) -> Vec<VHost> {
    #[derive(Default)]
    struct Block {
        spec_is_443: bool,
        domains: Vec<String>,
        cert_file: String,
        key_file: String,
        ca_file: Option<String>,
    }

    let vh_open = Regex::new(r"(?i)^<VirtualHost\s+([^>]+)>").unwrap();
    let mut cur: Option<Block> = None;
    let mut blocks: Vec<Block> = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(caps) = vh_open.captures(line) {
            let spec = &caps[1];
            cur = Some(Block { spec_is_443: spec.contains(':') && spec.contains("443"), ..Default::default() });
            continue;
        }
        if line.to_lowercase().starts_with("</virtualhost") {
            if let Some(b) = cur.take() {
                blocks.push(b);
            }
            continue;
        }
        let Some(b) = cur.as_mut() else { continue };
        let mut parts = line.splitn(2, char::is_whitespace);
        let Some(directive) = parts.next() else { continue };
        let rest = parts.next().unwrap_or("").trim();
        match directive.to_lowercase().as_str() {
            "servername" => {
                if !rest.is_empty() {
                    b.domains.insert(0, unquote(rest));
                }
            }
            "serveralias" => {
                b.domains.extend(rest.split_whitespace().map(|s| unquote(s).to_string()));
            }
            "sslcertificatefile" => b.cert_file = unquote(rest),
            "sslcertificatekeyfile" => b.key_file = unquote(rest),
            "sslcertificatechainfile" => b.ca_file = Some(unquote(rest)),
            _ => {}
        }
    }

    blocks
        .into_iter()
        .filter(|b| b.spec_is_443 && !b.cert_file.is_empty())
        .map(|b| VHost {
            domains: b.domains,
            cert_file: b.cert_file,
            key_file: b.key_file,
            ca_file: b.ca_file,
            config_file: config_file.to_string(),
            not_after: String::new(),
            days_left: None,
            cert_error: None,
        })
        .collect()
}

// ── shared: cert expiry ─────────────────────────────────────────────────

/// One round trip regardless of how many vhosts were found: batches every
/// unique cert path into a single `bash -c` script under one `sudo`
/// elevation, same shape as `kerneltune::engine::read_all_values`.
fn fill_cert_info(session: &ExecSession, mut vhosts: Vec<VHost>) -> Vec<VHost> {
    let mut paths: Vec<&str> = vhosts.iter().map(|v| v.cert_file.as_str()).collect();
    paths.sort_unstable();
    paths.dedup();
    if paths.is_empty() {
        return vhosts;
    }

    let mut script = String::from("echo \"NOW=$(date -u +%s)\"\n");
    for p in &paths {
        script.push_str(&format!(
            "echo \"FILE={p}\"\nend=$(openssl x509 -in {q} -noout -enddate 2>&1)\necho \"END=$end\"\nepoch=$(date -u -d \"${{end#notAfter=}}\" +%s 2>/dev/null)\necho \"EPOCH=$epoch\"\n",
            p = p,
            q = shell_quote(p)
        ));
    }
    let cmd = super::exec::sudo(&format!("bash -c {}", shell_quote(&script)));
    let Ok((out, _, _)) = session.run(&cmd) else { return vhosts };

    let mut now: i64 = 0;
    let mut cur_file = String::new();
    let mut cur_end = String::new();
    let mut per_file: std::collections::HashMap<String, (String, Option<i64>)> = std::collections::HashMap::new();
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("NOW=") {
            now = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("FILE=") {
            cur_file = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("END=") {
            cur_end = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("EPOCH=") {
            let epoch: Option<i64> = v.trim().parse().ok();
            per_file.insert(cur_file.clone(), (cur_end.clone(), epoch));
        }
    }

    for v in vhosts.iter_mut() {
        if let Some((end, epoch)) = per_file.get(&v.cert_file) {
            if let Some(e) = epoch {
                v.not_after = end.strip_prefix("notAfter=").unwrap_or(end).to_string();
                v.days_left = Some((e - now).div_euclid(86400));
            } else {
                v.cert_error = Some(crate::ssh_exec::one_line(end));
            }
        }
    }
    vhosts
}
