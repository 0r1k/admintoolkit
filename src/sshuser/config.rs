use serde::{Deserialize, Serialize};
use std::{fs, io};

use crate::config::{config_file, expand_path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub profiles: Vec<Profile>,
    pub default_port: String,
    pub default_ssh_user: String,
    pub default_ssh_key_path: String,
    pub default_ssh_password: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            profiles: Vec::new(),
            default_port: "22".into(),
            default_ssh_user: "root".into(),
            default_ssh_key_path: dirs::home_dir()
                .map(|h| h.join(".ssh/id_rsa").to_string_lossy().into_owned())
                .unwrap_or_default(),
            default_ssh_password: String::new(),
        }
    }
}

fn path() -> String {
    config_file("ssh_users.json").to_string_lossy().into_owned()
}

pub fn load() -> io::Result<Config> {
    let p = path();
    let expanded = expand_path(&p);
    if !std::path::Path::new(&expanded).exists() {
        let cfg = Config::default();
        save(&cfg)?;
        return Ok(cfg);
    }
    let data = fs::read_to_string(&expanded)?;
    serde_json::from_str(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn save(cfg: &Config) -> io::Result<()> {
    let p = path();
    let expanded = expand_path(&p);
    let json =
        serde_json::to_string_pretty(cfg).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    fs::write(&expanded, json)
}
