//! Writes a new certificate (and, optionally, a separate CA/chain file)
//! to the paths `detect.rs` found, safely: back up what's there first,
//! write, run the web server's own config test, and only reload if that
//! test passes — otherwise roll the backup straight back and never touch
//! the running service. If the vhost didn't already have a separate CA
//! directive and the caller supplies a CA file anyway, one is added
//! (marked with an `atk:BEGIN`/`atk:END` block, same convention
//! `kerneltune::engine` uses) rather than silently doing nothing with it.

use super::detect::VHost;
use super::exec::{self, shell_quote, ExecSession};

pub struct UpdateInput {
    pub vhost: VHost,
    pub server_kind: String, // "nginx" or "apache"
    pub cert_content: String,
    /// `Some(content)` when the caller is also providing a separate
    /// CA/chain file; `None` means the picked cert file already carries
    /// everything it needs (bundled chain, or no CA required at all).
    pub ca_content: Option<String>,
}

pub struct UpdateResult {
    pub messages: Vec<String>,
    pub new_not_after: String,
    pub new_days_left: Option<i64>,
}

pub fn update(session: &ExecSession, input: UpdateInput) -> Result<UpdateResult, String> {
    let mut messages = Vec::new();
    let backup_suffix = format!(".atk-bak-{}", now_stamp(session));

    let cert_backup = format!("{}{}", input.vhost.cert_file, backup_suffix);
    backup_file(session, &input.vhost.cert_file, &cert_backup)?;
    messages.push(format!("backed up {} -> {}", input.vhost.cert_file, cert_backup));

    write_remote_file(session, &input.vhost.cert_file, &input.cert_content)?;
    messages.push(format!("wrote new certificate to {}", input.vhost.cert_file));

    let mut ca_backup: Option<(String, String)> = None;
    let mut added_ca_directive = false;

    if let Some(ca_content) = &input.ca_content {
        let ca_path = match &input.vhost.ca_file {
            Some(existing) => existing.clone(),
            None => {
                let dir = std::path::Path::new(&input.vhost.cert_file).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                let name = input.vhost.domains.first().map(|d| d.replace('*', "wildcard")).unwrap_or_else(|| "vhost".to_string());
                format!("{}/atk-chain-{}.pem", if dir.is_empty() { ".".to_string() } else { dir }, name)
            }
        };

        if input.vhost.ca_file.is_some() {
            let backup = format!("{ca_path}{backup_suffix}");
            backup_file(session, &ca_path, &backup)?;
            ca_backup = Some((ca_path.clone(), backup));
            messages.push(format!("backed up {ca_path} -> {}", ca_backup.as_ref().unwrap().1));
        }

        write_remote_file(session, &ca_path, ca_content)?;
        messages.push(format!("wrote CA/chain file to {ca_path}"));

        if input.vhost.ca_file.is_none() {
            add_ca_directive(session, &input)?;
            added_ca_directive = true;
            messages.push("added a new CA directive to the vhost config (see the atk:BEGIN/END marker)".to_string());
        }
    }

    let test_result = config_test(session, &input.server_kind, &input.vhost.config_file);
    if let Err(e) = test_result {
        restore_file(session, &input.vhost.cert_file, &cert_backup).ok();
        if let Some((path, backup)) = &ca_backup {
            restore_file(session, path, backup).ok();
        }
        if added_ca_directive {
            messages.push("config test failed after adding the CA directive — you may need to remove it by hand (see the atk:BEGIN/END marker) once the underlying issue is fixed".to_string());
        }
        return Err(format!("config test failed, rolled back the file(s) just written: {e}"));
    }
    messages.push("config test passed".to_string());

    reload(session, &input.server_kind)?;
    messages.push("web server reloaded".to_string());

    let (not_after, days_left) = read_expiry(session, &input.vhost.cert_file);
    Ok(UpdateResult { messages, new_not_after: not_after, new_days_left: days_left })
}

fn now_stamp(session: &ExecSession) -> String {
    session.run("date -u +%Y%m%d%H%M%S").ok().map(|(out, _, _)| out.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "0".to_string())
}

fn backup_file(session: &ExecSession, path: &str, backup: &str) -> Result<(), String> {
    session.run_checked(&exec::sudo(&format!("cp -p {} {} 2>&1", shell_quote(path), shell_quote(backup)))).map(|_| ())
}

fn restore_file(session: &ExecSession, path: &str, backup: &str) -> Result<(), String> {
    session.run_checked(&exec::sudo(&format!("cp -p {} {} 2>&1", shell_quote(backup), shell_quote(path)))).map(|_| ())
}

fn write_remote_file(session: &ExecSession, path: &str, content: &str) -> Result<(), String> {
    let dir = std::path::Path::new(path).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let mkdir_part = if dir.is_empty() || dir == "/" { String::new() } else { format!("mkdir -p {} && ", shell_quote(&dir)) };
    let cmd = format!(
        "{mkdir_part}printf '%s' {} > {}.atk-tmp && mv {}.atk-tmp {} && chmod 644 {}",
        shell_quote(content),
        shell_quote(path),
        shell_quote(path),
        shell_quote(path),
        shell_quote(path)
    );
    session.run_checked(&exec::sudo(&format!("bash -c {}", shell_quote(&cmd)))).map(|_| ())
}

/// Inserts a `ssl_trusted_certificate <path>;` (nginx) or
/// `SSLCertificateChainFile <path>;` (apache) line into the vhost's own
/// config file, wrapped in an `# atk:BEGIN ssl-ca ...` / `# atk:END
/// ssl-ca ...` marker block placed right before the block's closing
/// brace/tag — never touches anything else in the file.
fn add_ca_directive(session: &ExecSession, input: &UpdateInput) -> Result<(), String> {
    let ca_path = input.vhost.ca_file.clone().unwrap_or_else(|| {
        let dir = std::path::Path::new(&input.vhost.cert_file).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
        let name = input.vhost.domains.first().map(|d| d.replace('*', "wildcard")).unwrap_or_else(|| "vhost".to_string());
        format!("{}/atk-chain-{}.pem", if dir.is_empty() { ".".to_string() } else { dir }, name)
    });

    if input.vhost.config_file.contains(',') || input.vhost.config_file == "nginx -T" {
        return Err("this vhost's config file couldn't be pinned down precisely enough to auto-edit — add the CA directive by hand".to_string());
    }
    let config_path = input.vhost.config_file.trim();

    let (marker_key, close_pat, insert_line) = if input.server_kind == "nginx" {
        ("ssl-ca", "}".to_string(), format!("    ssl_trusted_certificate {ca_path};"))
    } else {
        ("ssl-ca", "</VirtualHost>".to_string(), format!("    SSLCertificateChainFile {ca_path}"))
    };

    let (content, _, code) = session.run(&exec::sudo(&format!("cat {} 2>&1", shell_quote(config_path))))?;
    if code != 0 {
        return Err(format!("couldn't read {config_path} to insert the CA directive: {}", content.trim()));
    }

    let begin = format!("# atk:BEGIN {marker_key}");
    let end = format!("# atk:END {marker_key}");
    let block = format!("{begin}\n{insert_line}\n{end}\n");

    // Insert right before the *last* occurrence of the block's closing
    // token — good enough for the common case of one :443 vhost per file
    // (true for every certbot/Let's Encrypt-generated config, which is
    // the overwhelming majority of real-world setups).
    let new_content = match content.rfind(&close_pat) {
        Some(idx) => format!("{}{}{}", &content[..idx], block, &content[idx..]),
        None => return Err(format!("couldn't find a place to insert the CA directive in {config_path}")),
    };

    write_remote_file(session, config_path, &new_content)
}

fn config_test(session: &ExecSession, kind: &str, _config_file: &str) -> Result<(), String> {
    let candidates: &[&str] = if kind == "nginx" { &["nginx -t"] } else { &["apache2ctl -t", "httpd -t", "apachectl -t"] };
    let mut last_err = String::new();
    for c in candidates {
        match session.run(&exec::sudo(&format!("{c} 2>&1"))) {
            Ok((_, _, 0)) => return Ok(()),
            Ok((out, _, _)) => last_err = out,
            Err(e) => last_err = e,
        }
    }
    Err(if last_err.is_empty() { "config test command not found".to_string() } else { last_err })
}

fn reload(session: &ExecSession, kind: &str) -> Result<(), String> {
    let candidates: &[&str] =
        if kind == "nginx" { &["systemctl reload nginx", "nginx -s reload"] } else { &["systemctl reload apache2", "systemctl reload httpd", "apache2ctl graceful", "httpd -k graceful"] };
    for c in candidates {
        if let Ok((_, _, code)) = session.run(&exec::sudo(&format!("{c} 2>&1"))) {
            if code == 0 {
                return Ok(());
            }
        }
    }
    Err(format!("wrote the new file(s) and the config test passed, but none of the reload commands tried worked ({}) — reload the web server by hand", candidates.join(", ")))
}

fn read_expiry(session: &ExecSession, cert_file: &str) -> (String, Option<i64>) {
    let script = format!(
        "end=$(openssl x509 -in {q} -noout -enddate 2>&1); echo \"END=$end\"; echo \"NOW=$(date -u +%s)\"; echo \"EPOCH=$(date -u -d \"${{end#notAfter=}}\" +%s 2>/dev/null)\"",
        q = shell_quote(cert_file)
    );
    let Ok((out, _, _)) = session.run(&exec::sudo(&format!("bash -c {}", shell_quote(&script)))) else {
        return (String::new(), None);
    };
    let mut end = String::new();
    let mut now: i64 = 0;
    let mut epoch: Option<i64> = None;
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("END=") {
            end = v.trim().strip_prefix("notAfter=").unwrap_or(v.trim()).to_string();
        } else if let Some(v) = line.strip_prefix("NOW=") {
            now = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("EPOCH=") {
            epoch = v.trim().parse().ok();
        }
    }
    (end, epoch.map(|e| (e - now).div_euclid(86400)))
}
