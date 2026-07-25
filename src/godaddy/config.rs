use serde::{Deserialize, Serialize};
use std::{fs, io, time::{SystemTime, UNIX_EPOCH}};

use rand::RngCore;

use crate::config::config_file;

use crate::secret;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub label: String,
    pub api_key: String,
    pub api_secret_encrypted: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub accounts: Vec<Account>,
}

/// Decrypted view of an account, used only in-memory to call the GoDaddy API.
#[derive(Debug, Clone)]
pub struct AccountWithSecret {
    pub id: String,
    pub label: String,
    pub api_key: String,
    pub api_secret: String,
}

fn path() -> std::path::PathBuf {
    config_file("godaddy.json")
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
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    secs.to_string()
}

fn random_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Config {
    /// Create a new account, or update an existing one in place when `id`
    /// matches an account already on file.
    pub fn upsert_account(
        &mut self,
        id: Option<&str>,
        label: String,
        api_key: String,
        api_secret: String,
    ) -> io::Result<String> {
        let encrypted = secret::encrypt(&api_secret)?;
        let now = now_iso();

        if let Some(id) = id {
            if let Some(a) = self.accounts.iter_mut().find(|a| a.id == id) {
                a.label = label;
                a.api_key = api_key;
                a.api_secret_encrypted = encrypted;
                a.updated_at = now;
                return Ok(a.id.clone());
            }
        }

        let new_id = random_id();
        self.accounts.push(Account {
            id: new_id.clone(),
            label,
            api_key,
            api_secret_encrypted: encrypted,
            created_at: now.clone(),
            updated_at: now,
        });
        Ok(new_id)
    }

    pub fn delete_account(&mut self, id: &str) {
        self.accounts.retain(|a| a.id != id);
    }

    pub fn with_secret(&self, id: &str) -> Option<AccountWithSecret> {
        let a = self.accounts.iter().find(|a| a.id == id)?;
        let decrypted = secret::decrypt(&a.api_secret_encrypted).ok()?;
        Some(AccountWithSecret {
            id: a.id.clone(),
            label: a.label.clone(),
            api_key: a.api_key.clone(),
            api_secret: decrypted,
        })
    }
}
