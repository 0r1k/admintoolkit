use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    time::{SystemTime, UNIX_EPOCH},
};

use rand::RngCore;

use crate::config::config_file;
use crate::secret;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub label: String,
    /// DB host — if `use_tunnel` is set, this is resolved *from the SSH
    /// jump host's side* (e.g. an internal hostname/IP the DB isn't
    /// reachable at directly from here).
    pub host: String,
    pub port: String,
    pub db_user: String,
    #[serde(default)]
    pub db_password_encrypted: String,
    #[serde(default)]
    pub use_tunnel: bool,
    #[serde(default)]
    pub ssh_host: String,
    #[serde(default = "default_ssh_port")]
    pub ssh_port: String,
    #[serde(default)]
    pub ssh_user: String,
    #[serde(default)]
    pub ssh_key_path: String,
    #[serde(default)]
    pub ssh_password_encrypted: String,
    pub created_at: String,
    pub updated_at: String,
}

fn default_ssh_port() -> String {
    "22".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub connections: Vec<Connection>,
}

/// Decrypted view of a connection profile, used only in-memory to actually
/// connect to the database (and, if needed, the SSH jump host). `id`/`label`
/// are kept for API symmetry with the config side even though the DB
/// client itself never reads them.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ConnectionWithSecrets {
    pub id: String,
    pub label: String,
    pub host: String,
    pub port: String,
    pub db_user: String,
    pub db_password: String,
    pub use_tunnel: bool,
    pub ssh_host: String,
    pub ssh_port: String,
    pub ssh_user: String,
    pub ssh_key_path: String,
    pub ssh_password: String,
}

fn path() -> std::path::PathBuf {
    config_file("postgresql.json")
}

pub fn load() -> io::Result<Config> {
    let p = path();
    if !p.exists() {
        return Ok(Config::default());
    }
    let data = fs::read_to_string(&p)?;
    serde_json::from_str(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn save(cfg: &Config) -> io::Result<()> {
    let json =
        serde_json::to_string_pretty(cfg).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    fs::write(path(), json)
}

fn now_iso() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    secs.to_string()
}

fn random_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Everything the "Add / Edit Connection" form collects, before encryption.
pub struct ConnectionInput {
    pub label: String,
    pub host: String,
    pub port: String,
    pub db_user: String,
    pub db_password: String,
    pub use_tunnel: bool,
    pub ssh_host: String,
    pub ssh_port: String,
    pub ssh_user: String,
    pub ssh_key_path: String,
    pub ssh_password: String,
}

impl Config {
    /// Create a new connection, or update an existing one in place when
    /// `id` matches a connection already on file. Leaving `db_password`
    /// (or `ssh_password`) empty on an update keeps the previously stored
    /// one instead of blanking it out.
    pub fn upsert_connection(&mut self, id: Option<&str>, input: ConnectionInput) -> io::Result<String> {
        let now = now_iso();

        if let Some(id) = id {
            if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
                c.label = input.label;
                c.host = input.host;
                c.port = input.port;
                c.db_user = input.db_user;
                if !input.db_password.is_empty() {
                    c.db_password_encrypted = secret::encrypt(&input.db_password)?;
                }
                c.use_tunnel = input.use_tunnel;
                c.ssh_host = input.ssh_host;
                c.ssh_port = input.ssh_port;
                c.ssh_user = input.ssh_user;
                c.ssh_key_path = input.ssh_key_path;
                if !input.ssh_password.is_empty() {
                    c.ssh_password_encrypted = secret::encrypt(&input.ssh_password)?;
                }
                c.updated_at = now;
                return Ok(c.id.clone());
            }
        }

        let new_id = random_id();
        self.connections.push(Connection {
            id: new_id.clone(),
            label: input.label,
            host: input.host,
            port: input.port,
            db_user: input.db_user,
            db_password_encrypted: secret::encrypt_optional(&input.db_password)?,
            use_tunnel: input.use_tunnel,
            ssh_host: input.ssh_host,
            ssh_port: input.ssh_port,
            ssh_user: input.ssh_user,
            ssh_key_path: input.ssh_key_path,
            ssh_password_encrypted: secret::encrypt_optional(&input.ssh_password)?,
            created_at: now.clone(),
            updated_at: now,
        });
        Ok(new_id)
    }

    pub fn delete_connection(&mut self, id: &str) {
        self.connections.retain(|c| c.id != id);
    }

    pub fn with_secrets(&self, id: &str) -> Option<ConnectionWithSecrets> {
        let c = self.connections.iter().find(|c| c.id == id)?;
        Some(ConnectionWithSecrets {
            id: c.id.clone(),
            label: c.label.clone(),
            host: c.host.clone(),
            port: c.port.clone(),
            db_user: c.db_user.clone(),
            db_password: secret::decrypt_optional(&c.db_password_encrypted).ok()?,
            use_tunnel: c.use_tunnel,
            ssh_host: c.ssh_host.clone(),
            ssh_port: c.ssh_port.clone(),
            ssh_user: c.ssh_user.clone(),
            ssh_key_path: c.ssh_key_path.clone(),
            ssh_password: secret::decrypt_optional(&c.ssh_password_encrypted).ok()?,
        })
    }
}
