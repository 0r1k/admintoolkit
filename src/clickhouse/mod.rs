pub mod client;
pub mod config;
pub mod sql_client;

use crate::ssh_exec::Credentials;

/// SSH credentials for a connection profile — valid for SSH-XML mode
/// (`ssh_host` is the target server itself) and for SQL mode with
/// `use_tunnel` set (`ssh_host` is the jump host).
pub fn make_ssh_creds(cfg: &config::ConnectionWithSecrets) -> Credentials {
    Credentials {
        user: cfg.ssh_user.clone(),
        password: cfg.ssh_password.clone(),
        private_key_path: cfg.ssh_key_path.clone(),
    }
}
