//! Shared SSH plumbing used by the `sshuser` and `clickhouse` modules.
//! Wraps `ssh2` with password-or-key auth and a couple of exec helpers.

use ssh2::Session;
use std::{
    io::Read,
    net::{TcpStream, ToSocketAddrs},
    path::Path,
    time::Duration,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Debug, Default)]
pub struct Credentials {
    pub user: String,
    pub password: String,
    pub private_key_path: String,
}

fn connect_tcp(addr: &str, timeout: Duration) -> Result<TcpStream, String> {
    let mut last_err: Option<String> = None;
    for sa in addr.to_socket_addrs().map_err(|e| e.to_string())? {
        match TcpStream::connect_timeout(&sa, timeout) {
            Ok(tcp) => return Ok(tcp),
            Err(e) => last_err = Some(e.to_string()),
        }
    }
    Err(last_err.unwrap_or_else(|| format!("could not resolve {addr}")))
}

pub struct SshSession {
    sess: Session,
}

impl SshSession {
    pub fn connect(host: &str, port: &str, creds: &Credentials) -> Result<Self, String> {
        let addr = format!("{host}:{port}");
        let tcp = connect_tcp(&addr, CONNECT_TIMEOUT)?;
        tcp.set_read_timeout(Some(Duration::from_secs(30))).ok();
        tcp.set_write_timeout(Some(Duration::from_secs(10))).ok();

        let mut sess = Session::new().map_err(|e| e.to_string())?;
        sess.set_tcp_stream(tcp);
        sess.handshake().map_err(|e| e.to_string())?;

        if !creds.password.is_empty() {
            sess.userauth_password(&creds.user, &creds.password)
                .map_err(|e| e.to_string())?;
        } else if !creds.private_key_path.is_empty() {
            let key_path = crate::config::expand_path(&creds.private_key_path);
            sess.userauth_pubkey_file(&creds.user, None, Path::new(&key_path), None)
                .map_err(|e| e.to_string())?;
        } else {
            return Err("no auth method provided (set a password or a private key path)".into());
        }

        if !sess.authenticated() {
            return Err("authentication failed".into());
        }

        Ok(Self { sess })
    }

    /// Run a single remote command, returning (stdout, stderr, exit_status).
    pub fn exec_raw(&self, cmd: &str) -> Result<(String, String, i32), String> {
        let mut channel = self.sess.channel_session().map_err(|e| e.to_string())?;
        channel.exec(cmd).map_err(|e| e.to_string())?;

        let mut stdout = String::new();
        let mut stderr = String::new();
        channel.read_to_string(&mut stdout).ok();
        channel.stderr().read_to_string(&mut stderr).ok();
        channel.wait_close().ok();

        let exit_status = channel.exit_status().unwrap_or(1);
        Ok((stdout, stderr, exit_status))
    }

    /// Run a single command, treating a non-zero exit status as an error.
    pub fn exec_checked(&self, cmd: &str) -> Result<(String, String), String> {
        let (stdout, stderr, code) = self.exec_raw(cmd)?;
        if code != 0 {
            let msg = if !stderr.trim().is_empty() {
                stderr
            } else {
                format!("exit code {code}")
            };
            return Err(msg);
        }
        Ok((stdout, stderr))
    }

    /// Join several commands with `&&` and run them in one round trip
    /// inside `bash -lc '...'`. `stderr` may be non-empty even on success
    /// (e.g. `visudo -c` warns about undefined aliases but exits 0).
    pub fn exec_batch(&self, commands: &[String]) -> Result<(String, String), String> {
        let joined = commands.join(" && ");
        let cmd = format!("bash -lc '{}'", escape_single_quotes(&joined));
        self.exec_checked(&cmd)
    }

    /// Opens a `direct-tcpip` channel: the SSH server connects onward to
    /// `host:port` on its side and hands back a channel that behaves like a
    /// plain TCP stream tunneled over this SSH connection. Used by
    /// `ssh_tunnel` to forward a local port to a database behind a jump
    /// host.
    pub fn direct_tcpip(&self, host: &str, port: u16) -> Result<ssh2::Channel, String> {
        self.sess
            .channel_direct_tcpip(host, port, None)
            .map_err(|e| e.to_string())
    }

    /// Toggles blocking mode for every channel opened on this session —
    /// needed by the tunnel's byte-pump loop, which polls both the local
    /// socket and the SSH channel without either side being allowed to
    /// block the other.
    pub fn set_blocking(&self, blocking: bool) {
        self.sess.set_blocking(blocking);
    }
}

/// One-shot connect + batch-exec, matching the old sshuser::run_commands signature.
pub fn run_commands(
    host: &str,
    port: &str,
    creds: &Credentials,
    commands: &[String],
) -> Result<(String, String), String> {
    let sess = SshSession::connect(host, port, creds)?;
    sess.exec_batch(commands)
}

pub fn escape_single_quotes(s: &str) -> String {
    s.replace('\'', r"'\''")
}

pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
