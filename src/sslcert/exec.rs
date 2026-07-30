//! Runs shell commands on a remote host over SSH. Remote-only, unlike
//! `kerneltune::exec` (which this is otherwise modeled on): a certificate
//! always belongs to the server being administered, never to whatever
//! machine happens to be running `atk` itself, so there's no Local mode
//! to offer here.

use crate::ssh_exec::{Credentials, SshSession};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Remote { host: String, port: String, user: String, key_path: String, password: String },
}

impl Target {
    pub fn label(&self) -> String {
        let Target::Remote { host, user, .. } = self;
        if user.is_empty() {
            host.clone()
        } else {
            format!("{user}@{host}")
        }
    }
}

pub struct ExecSession {
    sess: SshSession,
}

impl ExecSession {
    pub fn open(target: &Target) -> Result<Self, String> {
        let Target::Remote { host, port, user, key_path, password } = target;
        let creds = Credentials { user: user.clone(), password: password.clone(), private_key_path: key_path.clone() };
        let port = if port.trim().is_empty() { "22" } else { port.trim() };
        let sess = SshSession::connect(host, port, &creds)?;
        Ok(Self { sess })
    }

    /// Runs one command, returning (stdout, stderr, exit_code) — never an
    /// `Err` just because the command itself exited non-zero, only for
    /// transport-level failures (SSH channel error).
    pub fn run(&self, cmd: &str) -> Result<(String, String, i32), String> {
        self.sess.exec_raw(cmd)
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

/// `sudo -n` (never prompt) — see `kerneltune::exec::sudo` for why: over a
/// non-interactive SSH channel a password prompt would just hang.
pub fn sudo(cmd: &str) -> String {
    format!("sudo -n {cmd}")
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}
