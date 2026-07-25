use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    time::{SystemTime, UNIX_EPOCH},
};

use rand::RngCore;

use crate::config::config_file;
use crate::secret;

pub const TAG_AUTO: &str = "auto";
pub const TAG_CLICKHOUSE: &str = "clickhouse";
pub const TAG_YANDEX: &str = "yandex";
pub const TAG_MODES: &[&str] = &[TAG_AUTO, TAG_CLICKHOUSE, TAG_YANDEX];

/// Direct SQL over ClickHouse's HTTP interface (`CREATE USER`/`ALTER
/// USER`/`DROP USER`), vs. the legacy approach of SSHing into the box and
/// hand-editing `/etc/clickhouse-server/users.d/*.xml` — some deployments
/// still provision users exclusively through that XML tree, so both need
/// to stay available side by side.
pub const MODE_SQL: &str = "sql";
pub const MODE_SSH_XML: &str = "ssh_xml";

fn default_mode() -> String {
    MODE_SQL.to_string()
}

fn default_sql_port() -> String {
    "8123".to_string()
}

fn default_ssh_port() -> String {
    "22".to_string()
}

fn default_tag_mode() -> String {
    TAG_AUTO.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub label: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    /// SQL mode: the ClickHouse HTTP host — if `use_tunnel` is set, this is
    /// resolved *from the SSH jump host's side*. Unused in SSH-XML mode,
    /// where `ssh_host` itself is the target server.
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_sql_port")]
    pub port: String,
    #[serde(default)]
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
    /// SSH-XML mode only: which root tag the generated `users.d/*.xml`
    /// files use (`<clickhouse>` vs. legacy `<yandex>`).
    #[serde(default = "default_tag_mode")]
    pub tag_mode: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub connections: Vec<Connection>,
}

/// Decrypted view of a connection profile, used only in-memory to actually
/// connect (either straight to ClickHouse's HTTP interface, or over SSH).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ConnectionWithSecrets {
    pub id: String,
    pub label: String,
    pub mode: String,
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
    pub tag_mode: String,
}

impl ConnectionWithSecrets {
    pub fn is_sql(&self) -> bool {
        self.mode == MODE_SQL
    }
}

fn path() -> std::path::PathBuf {
    config_file("clickhouse.json")
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
    pub mode: String,
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
    pub tag_mode: String,
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
                c.mode = input.mode;
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
                c.tag_mode = input.tag_mode;
                c.updated_at = now;
                return Ok(c.id.clone());
            }
        }

        let new_id = random_id();
        self.connections.push(Connection {
            id: new_id.clone(),
            label: input.label,
            mode: input.mode,
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
            tag_mode: input.tag_mode,
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
            mode: c.mode.clone(),
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
            tag_mode: c.tag_mode.clone(),
        })
    }
}
