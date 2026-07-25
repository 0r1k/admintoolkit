use std::path::PathBuf;

/// Shared config directory for every atk module: `~/.config/admintoolkit/`
/// (platform equivalents via `dirs::config_dir`). Each module keeps its own
/// file inside it (e.g. `ssh_users.json`, `clickhouse.json`, `godaddy.json`)
/// so tools stay independent while sharing one place on disk.
pub fn config_dir() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("admintoolkit");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn config_file(name: &str) -> PathBuf {
    config_dir().join(name)
}

pub fn expand_path(path: &str) -> String {
    let path = if let Some(rest) = path.strip_prefix('~') {
        if let Some(home) = dirs::home_dir() {
            format!("{}{rest}", home.display())
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };
    let mut result = path;
    for (key, val) in std::env::vars() {
        result = result.replace(&format!("${key}"), &val);
        result = result.replace(&format!("${{{key}}}"), &val);
    }
    result
}
