//! Local port forwarding over SSH ("ssh -L"-style tunnel), used by the
//! MySQL and PostgreSQL user managers to reach a database that only
//! listens on a jump host's internal network. Any DB client that can
//! connect to a plain TCP host:port can use this transparently — it just
//! points at `127.0.0.1:<local_port>`.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::Duration,
};

use crate::ssh_exec::{Credentials, SshSession};

pub struct Tunnel {
    pub local_port: u16,
}

/// Opens an SSH connection to `ssh_host:ssh_port` and starts forwarding a
/// freshly bound local port to `target_host:target_port` as seen from the
/// SSH server. Blocks until the SSH handshake completes and the local
/// listener is bound, then returns immediately — the actual forwarding
/// (accepting the one connection the caller's DB client makes, and
/// pumping bytes both ways) happens in a background thread for the
/// lifetime of that connection.
pub fn open(
    ssh_host: &str,
    ssh_port: &str,
    ssh_creds: &Credentials,
    target_host: &str,
    target_port: u16,
) -> Result<Tunnel, String> {
    let sess = SshSession::connect(ssh_host, ssh_port, ssh_creds)?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let local_port = listener.local_addr().map_err(|e| e.to_string())?.port();

    let target_host = target_host.to_string();
    thread::spawn(move || {
        // A DB client opens exactly one connection per session, which is
        // all every caller here needs; accept just that one and forward it.
        if let Ok((local_stream, _)) = listener.accept() {
            let _ = pump(&sess, local_stream, &target_host, target_port);
        }
    });

    Ok(Tunnel { local_port })
}

fn pump(sess: &SshSession, mut local: TcpStream, target_host: &str, target_port: u16) -> Result<(), String> {
    // Open the channel while the session is still blocking: establishing
    // it takes several synchronous round trips with the SSH server, which
    // non-blocking mode would just fail outright rather than retry.
    let mut channel = sess.direct_tcpip(target_host, target_port)?;

    // Only now switch both sides to non-blocking, for the pump loop below.
    sess.set_blocking(false);
    local.set_nonblocking(true).map_err(|e| e.to_string())?;

    let mut buf = [0u8; 16 * 1024];
    loop {
        let mut activity = false;

        match local.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if write_all_retrying(&mut channel, &buf[..n]).is_err() {
                    break;
                }
                activity = true;
            }
            Err(e) if would_block(&e) => {}
            Err(_) => break,
        }

        match channel.read(&mut buf) {
            Ok(0) => {
                if channel.eof() {
                    break;
                }
            }
            Ok(n) => {
                if write_all_retrying(&mut local, &buf[..n]).is_err() {
                    break;
                }
                activity = true;
            }
            Err(e) if would_block(&e) => {}
            Err(_) => break,
        }

        if channel.eof() {
            break;
        }
        if !activity {
            thread::sleep(Duration::from_millis(5));
        }
    }

    let _ = channel.close();
    Ok(())
}

fn would_block(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock
}

/// `Write::write_all` treats `WouldBlock` as a hard error, which is wrong
/// on a non-blocking stream — it just means "try again in a moment", not
/// "the connection is broken". Retry on `WouldBlock` instead of bailing.
fn write_all_retrying<W: Write>(w: &mut W, mut data: &[u8]) -> Result<(), String> {
    while !data.is_empty() {
        match w.write(data) {
            Ok(0) => return Err("write returned 0".to_string()),
            Ok(n) => data = &data[n..],
            Err(e) if would_block(&e) => thread::sleep(Duration::from_millis(2)),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}
