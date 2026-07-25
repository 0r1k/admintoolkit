//! Reads, applies, persists and reverts tunables from `catalog.rs` against
//! whatever `ExecSession` (local or remote) is handed in. This is where the
//! "runtime-only unless the user opts into persisting" rule actually lives:
//! `apply_change` only ever touches `/proc`/`sysctl -w`/live sysfs state
//! unless `persist` is `true`, in which case it *additionally* writes a
//! dedicated, clearly-marked drop-in file — the main system config
//! (`/etc/sysctl.conf`, `/etc/security/limits.conf`) is never touched
//! directly, so nothing this tool does can silently clobber an unrelated
//! setting someone else already had in place.

use std::collections::HashMap;

use super::catalog::{Kind, Tunable};
use super::exec::{self, shell_quote, ExecSession};

const SYSCTL_DROPIN: &str = "/etc/sysctl.d/99-atk-tuning.conf";
const LIMITS_DROPIN: &str = "/etc/security/limits.d/99-atk-tuning.conf";
const SYSFS_SCRIPT: &str = "/etc/atk-kerneltune/apply.sh";
const SYSFS_UNIT: &str = "/etc/systemd/system/atk-kerneltune.service";

#[derive(Debug, Clone, Default)]
pub struct TargetInfo {
    pub os_pretty: String,
    pub kernel: String,
    pub arch: String,
    pub cpus: String,
    pub mem_total_kb: String,
}

/// Read-only, no sudo needed — os-release/uname/nproc/meminfo are all
/// world-readable.
pub fn probe_target(session: &ExecSession) -> Result<TargetInfo, String> {
    let cmd = r#"echo "OS=$(. /etc/os-release 2>/dev/null; echo "$PRETTY_NAME")"; echo "KERNEL=$(uname -r)"; echo "ARCH=$(uname -m)"; echo "CPUS=$(nproc 2>/dev/null)"; echo "MEM=$(awk '/MemTotal/{print $2}' /proc/meminfo 2>/dev/null)""#;
    let (out, _) = session.run_checked(cmd)?;
    let mut info = TargetInfo::default();
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("OS=") {
            info.os_pretty = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("KERNEL=") {
            info.kernel = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("ARCH=") {
            info.arch = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("CPUS=") {
            info.cpus = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("MEM=") {
            info.mem_total_kb = v.trim().to_string();
        }
    }
    Ok(info)
}

/// Reads the live current value of every tunable in one pass: one batched
/// `sysctl -e` call for all `Sysctl` keys, one small generated script for
/// all `Sysfs` globs, one `cat` of the limits drop-in for all `Limits`
/// entries — three round trips total regardless of catalog size, not one
/// per key. Keys with no readable value (module not loaded, path doesn't
/// exist on this kernel, nothing persisted yet) are simply absent from the
/// map; callers show that as "n/a" rather than treating it as an error.
pub fn read_all_values(session: &ExecSession, tunables: &[&'static Tunable]) -> HashMap<String, String> {
    let mut out = HashMap::new();

    let sysctl_keys: Vec<&str> = tunables.iter().filter(|t| matches!(t.kind, Kind::Sysctl)).map(|t| t.key).collect();
    if !sysctl_keys.is_empty() {
        let cmd = format!("sysctl -e {} 2>/dev/null", sysctl_keys.join(" "));
        if let Ok((stdout, _, _)) = session.run(&cmd) {
            for line in stdout.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    out.insert(k.trim().to_string(), normalize_value(v));
                }
            }
        }
    }

    let sysfs: Vec<(&str, &str)> =
        tunables.iter().filter_map(|t| if let Kind::Sysfs(glob) = t.kind { Some((t.key, glob)) } else { None }).collect();
    if !sysfs.is_empty() {
        let mut script = String::new();
        for (key, glob) in &sysfs {
            script.push_str(&format!(
                "for f in {glob}; do [ -e $f ] && {{ echo \"{key}=$(cat $f 2>/dev/null)\"; break; }}; done\n"
            ));
        }
        let cmd = format!("bash -c {}", shell_quote(&script));
        if let Ok((stdout, _, _)) = session.run(&cmd) {
            for line in stdout.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    out.insert(k.trim().to_string(), normalize_value(v));
                }
            }
        }
    }

    let limits: Vec<(&'static str, &'static str, &'static str)> = tunables
        .iter()
        .filter_map(|t| if let Kind::Limits { domain, item } = t.kind { Some((t.key, domain, item)) } else { None })
        .collect();
    if !limits.is_empty() {
        let content = read_remote_file(session, LIMITS_DROPIN);
        for (key, domain, item) in limits {
            let needle = format!("{domain} soft {item} ");
            if let Some(line) = content.lines().find(|l| l.trim().starts_with(&needle)) {
                if let Some(v) = line.trim().strip_prefix(&needle) {
                    out.insert(key.to_string(), v.trim().to_string());
                }
            }
        }
    }

    out
}

/// `sysctl -e`'s multi-value output (e.g. `net.ipv4.tcp_rmem = 4096\t131072\t33554432`)
/// separates values with a raw tab character, not spaces. Rendered as-is
/// in a table cell, that literal `\t` byte makes a real terminal jump to
/// the next 8-column tab stop instead of behaving like a normal
/// fixed-width character — blowing straight through whatever column width
/// the table intended and corrupting the whole row's alignment. Collapsing
/// all internal whitespace to single spaces up front (matching how this
/// catalog's own recommended-value strings are formatted, e.g.
/// `"4096 87380 134217728"`) avoids that regardless of which command a
/// value came from.
fn normalize_value(v: &str) -> String {
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Applies `value` live. For `Sysctl`/`Sysfs` this only ever changes
/// running kernel state — nothing on disk. `Limits` has no live state
/// (PAM only reads limits.d at login), so applying one always means
/// writing the file; that's inherent to what a ulimit is, not something
/// this tool can work around.
pub fn apply_runtime(session: &ExecSession, t: &Tunable, value: &str) -> Result<String, String> {
    match t.kind {
        Kind::Sysctl => {
            let cmd = exec::sudo(&format!("sysctl -w {} 2>&1", shell_quote(&format!("{}={}", t.key, value))));
            let (out, _) = session.run_checked(&cmd)?;
            Ok(out.trim().to_string())
        }
        Kind::Sysfs(glob) => {
            let script = format!(
                "n=0; for f in {glob}; do [ -e \"$f\" ] || continue; n=$((n+1)); printf '%s\\n' {val} > \"$f\" 2>/dev/null || echo \"FAILED:$f\"; done; echo \"OK:$n\"",
                val = shell_quote(value)
            );
            let cmd = exec::sudo(&format!("bash -c {}", shell_quote(&script)));
            let (out, err) = session.run_checked(&cmd)?;
            if out.contains("FAILED:") {
                return Err(format!("some device(s) rejected the value: {} {}", out.trim(), err.trim()));
            }
            if out.contains("OK:0") {
                return Err("no matching device found on this target".to_string());
            }
            Ok(out.trim().to_string())
        }
        Kind::Limits { .. } => {
            persist(session, t, value)?;
            Ok("written to limits.d (takes effect on next login, not this session)".to_string())
        }
    }
}

/// Writes `value` to atk's own drop-in file/script so it survives a
/// reboot, on top of (not instead of) `apply_runtime`. Never touches
/// `/etc/sysctl.conf` or `/etc/security/limits.conf` themselves.
pub fn persist(session: &ExecSession, t: &Tunable, value: &str) -> Result<(), String> {
    match t.kind {
        Kind::Sysctl => {
            let content = read_remote_file(session, SYSCTL_DROPIN);
            let content = ensure_header(
                content,
                "# Managed by atk's Kernel Tuner. Blocks between '# atk:BEGIN <key>' and\n# '# atk:END <key>' are safe to remove by hand or via the app's History tab.\n",
            );
            let new_content = upsert_block(&content, t.key, &[format!("{} = {}", t.key, value)]);
            write_remote_file(session, SYSCTL_DROPIN, &new_content)?;
            session
                .run_checked(&exec::sudo("sysctl --system > /dev/null 2>&1"))
                .map_err(|e| format!("wrote {SYSCTL_DROPIN} but `sysctl --system` reload failed: {e}"))?;
            Ok(())
        }
        Kind::Sysfs(glob) => {
            ensure_sysfs_unit_installed(session)?;
            let content = read_remote_file(session, SYSFS_SCRIPT);
            let content = ensure_header(
                content,
                "#!/bin/bash\n# Managed by atk's Kernel Tuner — replays sysfs tunables at boot, since sysfs\n# has no sysctl.d-equivalent persistence mechanism of its own. Safe to hand-edit.\n",
            );
            let line = format!("for f in {glob}; do [ -e \"$f\" ] && printf '%s\\n' {} > \"$f\"; done", shell_quote(value));
            let new_content = upsert_block(&content, t.key, &[line]);
            write_remote_file(session, SYSFS_SCRIPT, &new_content)?;
            session.run_checked(&exec::sudo(&format!("chmod 755 {SYSFS_SCRIPT}")))?;
            Ok(())
        }
        Kind::Limits { domain, item } => {
            let content = read_remote_file(session, LIMITS_DROPIN);
            let content = ensure_header(content, "# Managed by atk's Kernel Tuner.\n");
            let lines = vec![format!("{domain} soft {item} {value}"), format!("{domain} hard {item} {value}")];
            let new_content = upsert_block(&content, t.key, &lines);
            write_remote_file(session, LIMITS_DROPIN, &new_content)?;
            Ok(())
        }
    }
}

/// Applies live and, if `persist_it`, also persists — the one entry point
/// the screen actually calls per staged change. `Limits` tunables ignore
/// `persist_it` (see `apply_runtime`'s doc comment) since there is no
/// non-persisted form of a ulimit.
pub fn apply_change(session: &ExecSession, t: &Tunable, value: &str, persist_it: bool) -> Result<String, String> {
    let msg = apply_runtime(session, t, value)?;
    if persist_it && !matches!(t.kind, Kind::Limits { .. }) {
        persist(session, t, value)?;
    }
    Ok(msg)
}

/// Restores `previous_value` at runtime (no-op for `Limits`, which has no
/// runtime state) and, if it had been persisted, removes atk's entry for
/// it from the drop-in file/script.
pub fn revert(session: &ExecSession, t: &Tunable, previous_value: &str, was_persisted: bool) -> Result<(), String> {
    if !matches!(t.kind, Kind::Limits { .. }) {
        apply_runtime(session, t, previous_value)?;
    }
    if was_persisted || matches!(t.kind, Kind::Limits { .. }) {
        remove_persisted(session, t)?;
    }
    Ok(())
}

fn remove_persisted(session: &ExecSession, t: &Tunable) -> Result<(), String> {
    match t.kind {
        Kind::Sysctl => {
            let content = read_remote_file(session, SYSCTL_DROPIN);
            let new_content = upsert_block(&content, t.key, &[]);
            write_remote_file(session, SYSCTL_DROPIN, &new_content)?;
            session.run_checked(&exec::sudo("sysctl --system > /dev/null 2>&1")).ok();
            Ok(())
        }
        Kind::Sysfs(_) => {
            let content = read_remote_file(session, SYSFS_SCRIPT);
            let new_content = upsert_block(&content, t.key, &[]);
            write_remote_file(session, SYSFS_SCRIPT, &new_content)?;
            Ok(())
        }
        Kind::Limits { .. } => {
            let content = read_remote_file(session, LIMITS_DROPIN);
            let new_content = upsert_block(&content, t.key, &[]);
            write_remote_file(session, LIMITS_DROPIN, &new_content)?;
            Ok(())
        }
    }
}

fn ensure_sysfs_unit_installed(session: &ExecSession) -> Result<(), String> {
    const UNIT: &str = "[Unit]\nDescription=atk kernel tuner - reapply persisted sysfs tunables at boot\nAfter=multi-user.target\n\n[Service]\nType=oneshot\nExecStart=/bin/bash /etc/atk-kerneltune/apply.sh\nRemainAfterExit=yes\n\n[Install]\nWantedBy=multi-user.target\n";
    let current = read_remote_file(session, SYSFS_UNIT);
    if current.trim() != UNIT.trim() {
        write_remote_file(session, SYSFS_UNIT, UNIT)?;
        session.run_checked(&exec::sudo("systemctl daemon-reload")).ok();
    }
    session.run_checked(&exec::sudo("systemctl enable atk-kerneltune.service")).ok();
    Ok(())
}

fn read_remote_file(session: &ExecSession, path: &str) -> String {
    session.run(&exec::sudo(&format!("cat {path} 2>/dev/null"))).map(|(out, _, _)| out).unwrap_or_default()
}

fn write_remote_file(session: &ExecSession, path: &str, content: &str) -> Result<(), String> {
    let dir = std::path::Path::new(path).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let mkdir_part = if dir.is_empty() || dir == "/" { String::new() } else { format!("sudo -n mkdir -p {} && ", shell_quote(&dir)) };
    let cmd =
        format!("{mkdir_part}printf '%s' {} | sudo -n tee {path} > /dev/null && sudo -n chmod 644 {path}", shell_quote(content));
    session.run_checked(&cmd)?;
    Ok(())
}

fn ensure_header(content: String, header: &str) -> String {
    if content.trim().is_empty() {
        header.to_string()
    } else {
        content
    }
}

/// Removes any existing `# atk:BEGIN <key>` .. `# atk:END <key>` block and,
/// if `new_lines` is non-empty, appends a fresh one — the same marker
/// scheme is reused for the sysctl drop-in, the limits drop-in, and the
/// sysfs replay script, so this one function handles upsert *and* removal
/// (an empty `new_lines` removes without replacing) for all three.
fn upsert_block(content: &str, key: &str, new_lines: &[String]) -> String {
    let begin = format!("# atk:BEGIN {key}");
    let end = format!("# atk:END {key}");
    let mut out: Vec<String> = Vec::new();
    let mut skipping = false;
    for line in content.lines() {
        let t = line.trim();
        if t == begin {
            skipping = true;
            continue;
        }
        if t == end {
            skipping = false;
            continue;
        }
        if skipping {
            continue;
        }
        out.push(line.to_string());
    }
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    if !new_lines.is_empty() {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(begin);
        out.extend(new_lines.iter().cloned());
        out.push(end);
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}
