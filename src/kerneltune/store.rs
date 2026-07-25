//! Local (on the machine running atk, not the target) undo log of every
//! change the Kernel Tuner has applied, keyed by target label. Exists so
//! "what did I change on this box" and "revert it" both survive atk
//! restarting — the target itself only ever holds the live value plus,
//! optionally, atk's own drop-in files; the *history* of what the previous
//! value was lives here.

use std::fs;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::config_file;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub target: String,
    pub key: String,
    pub title: String,
    pub previous_value: String,
    pub new_value: String,
    pub persisted: bool,
    pub when: String,
}

fn path() -> std::path::PathBuf {
    config_file("kerneltune_history.json")
}

pub fn load() -> Vec<HistoryEntry> {
    let p = path();
    if !p.exists() {
        return Vec::new();
    }
    fs::read_to_string(&p).ok().and_then(|data| serde_json::from_str(&data).ok()).unwrap_or_default()
}

pub fn save(entries: &[HistoryEntry]) -> io::Result<()> {
    let json = serde_json::to_string_pretty(entries).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    fs::write(path(), json)
}

fn now_iso() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    secs.to_string()
}

/// Records a successful apply. If atk already had an entry for this
/// `(target, key)` (e.g. the value was changed twice in a row without
/// reverting), it's replaced rather than duplicated — reverting always
/// means "go back to how it was before atk first touched this", not
/// "undo one step at a time".
pub fn record(mut entries: Vec<HistoryEntry>, target: &str, t: &crate::kerneltune::catalog::Tunable, previous_value: &str, new_value: &str, persisted: bool) -> Vec<HistoryEntry> {
    entries.retain(|e| !(e.target == target && e.key == t.key));
    entries.push(HistoryEntry {
        target: target.to_string(),
        key: t.key.to_string(),
        title: t.title.to_string(),
        previous_value: previous_value.to_string(),
        new_value: new_value.to_string(),
        persisted,
        when: now_iso(),
    });
    let _ = save(&entries);
    entries
}

pub fn remove(mut entries: Vec<HistoryEntry>, target: &str, key: &str) -> Vec<HistoryEntry> {
    entries.retain(|e| !(e.target == target && e.key == key));
    let _ = save(&entries);
    entries
}
