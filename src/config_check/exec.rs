//! Runs shell commands either on localhost (`std::process::Command`) or on
//! a remote host (a plain `SshSession`) — identical shape to
//! `kerneltune::exec`, duplicated rather than shared since every atk module
//! that needs a local-or-remote runner keeps its own copy (see
//! `sslcert::exec`, `kerneltune::exec`). Config files can live on either
//! side (checking a local app config vs. a remote server's `/etc/...`), so
//! unlike `sslcert::exec` this one offers both.

use std::process::Command;

use crate::ssh_exec::{Credentials, SshSession};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Local,
    Remote { host: String, port: String, user: String, key_path: String, password: String },
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Target::Local => "localhost".to_string(),
            Target::Remote { host, user, .. } => {
                if user.is_empty() {
                    host.clone()
                } else {
                    format!("{user}@{host}")
                }
            }
        }
    }

}

enum Inner {
    Local,
    Remote(SshSession),
}

pub struct ExecSession {
    inner: Inner,
}

impl ExecSession {
    pub fn open(target: &Target) -> Result<Self, String> {
        match target {
            Target::Local => Ok(Self { inner: Inner::Local }),
            Target::Remote { host, port, user, key_path, password } => {
                let creds = Credentials { user: user.clone(), password: password.clone(), private_key_path: key_path.clone() };
                let port = if port.trim().is_empty() { "22" } else { port.trim() };
                let sess = SshSession::connect(host, port, &creds)?;
                Ok(Self { inner: Inner::Remote(sess) })
            }
        }
    }

    /// Runs one command, returning (stdout, stderr, exit_code) — never an
    /// `Err` just because the command itself exited non-zero, only for
    /// transport-level failures (can't spawn locally, SSH channel error).
    pub fn run(&self, cmd: &str) -> Result<(String, String, i32), String> {
        match &self.inner {
            Inner::Local => run_local(cmd),
            Inner::Remote(sess) => sess.exec_raw(cmd),
        }
    }

    /// Like `run`, but turns a non-zero exit code into `Err` (stderr if
    /// present, else stdout, else the bare exit code) — the shape most
    /// call sites want.
    pub fn run_checked(&self, cmd: &str) -> Result<(String, String), String> {
        let (stdout, stderr, code) = self.run(cmd)?;
        if code != 0 {
            let msg = if !stderr.trim().is_empty() {
                stderr
            } else if !stdout.trim().is_empty() {
                stdout
            } else {
                format!("exit code {code}")
            };
            return Err(msg);
        }
        Ok((stdout, stderr))
    }

    /// Like `run_checked`, but retries once with `sudo -n` if the plain
    /// attempt fails — most config files a sysadmin wants checked are
    /// root-owned (`/etc/nginx/...`), and there's no separate "use sudo"
    /// toggle in the UI to keep the form simple, so this is transparent:
    /// try as the login user first, only escalate if that didn't work.
    /// Returns the plain attempt's error if the sudo retry also fails —
    /// it's usually the more informative one (e.g. "No such file or
    /// directory" vs. sudo's own "a password is required").
    pub fn run_checked_sudo(&self, cmd: &str) -> Result<(String, String), String> {
        match self.run_checked(cmd) {
            Ok(v) => Ok(v),
            Err(plain_err) => self.run_checked(&sudo(cmd)).map_err(|_| plain_err),
        }
    }
}

fn run_local(cmd: &str) -> Result<(String, String, i32), String> {
    let output = Command::new("bash").arg("-lc").arg(cmd).output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(1);
    Ok((stdout, stderr, code))
}

/// `sudo -n` (never prompt) — see `kerneltune::exec::sudo` for why: over a
/// non-interactive SSH channel (or this app's own raw-mode terminal for the
/// Local case) an interactive password prompt would just hang.
pub fn sudo(cmd: &str) -> String {
    format!("sudo -n {cmd}")
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}
