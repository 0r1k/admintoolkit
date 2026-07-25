//! Runs shell commands either on localhost (`std::process::Command`) or on
//! a remote host (a plain `SshSession`, same as every other atk module).
//! An `ExecSession` is opened once per operation (inside whatever thread
//! does the work) and reused for every command in that operation, so a
//! multi-step apply/persist sequence costs one SSH handshake, not one per
//! command — the same "connect once, reuse the session" shape every other
//! screen already uses via `client::connect(&cfg)` inside `thread::spawn`.

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
}

fn run_local(cmd: &str) -> Result<(String, String, i32), String> {
    let output = Command::new("bash").arg("-lc").arg(cmd).output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(1);
    Ok((stdout, stderr, code))
}

/// `sudo -n` (never prompt) everywhere in this module rather than the
/// plain `sudo -i` other atk modules use over a non-interactive SSH
/// channel. Over SSH without a pty, an interactive password prompt would
/// just hang waiting for input that can never arrive; run locally inside
/// this TUI's raw-mode terminal, a prompt would additionally corrupt the
/// screen and freeze the whole event loop (this call blocks synchronously
/// on the main thread's behalf). `-n` makes a missing NOPASSWD sudo rule
/// fail fast with a clear error instead of either of those.
pub fn sudo(cmd: &str) -> String {
    format!("sudo -n {cmd}")
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}
