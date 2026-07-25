pub mod commands;
pub mod config;

use crate::ssh_exec::Credentials;

pub fn make_creds(cfg: &config::Config) -> Credentials {
    Credentials {
        user: cfg.default_ssh_user.clone(),
        password: cfg.default_ssh_password.clone(),
        private_key_path: cfg.default_ssh_key_path.clone(),
    }
}

pub fn find_profile<'a>(cfg: &'a config::Config, name: &str) -> Option<&'a config::Profile> {
    cfg.profiles.iter().find(|p| p.name == name)
}
