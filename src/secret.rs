//! At-rest encryption for secrets this app stores on disk: GoDaddy API
//! secrets, and MySQL/PostgreSQL/SSH passwords for the DB user managers.
//! There is no cross-platform equivalent of Electron's OS-backed
//! `safeStorage` without pulling in a native keyring dependency
//! (libsecret/dbus on Linux), so instead we encrypt with AES-256-GCM under
//! a random key generated on first use and stored at
//! `~/.config/admintoolkit/.godaddy.key` with `0600` permissions
//! (owner-only — the filename predates this module covering more than
//! GoDaddy, kept as-is so existing encrypted secrets keep decrypting).
//! This protects secrets from casual disk browsing / backups that don't
//! also capture the key file, but — unlike an OS keychain — the key lives
//! next to the data, so it's not a defense against another process
//! running as the same user.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::{engine::general_purpose::STANDARD, Engine};
use rand::RngCore;
use std::{fs, io, path::PathBuf};

fn key_path() -> PathBuf {
    crate::config::config_file(".godaddy.key")
}

fn load_or_create_key() -> io::Result<[u8; 32]> {
    let path = key_path();
    if let Ok(data) = fs::read(&path) {
        if data.len() == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&data);
            return Ok(key);
        }
    }

    let mut key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    fs::write(&path, key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}

fn err(msg: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg.to_string())
}

pub fn encrypt(plaintext: &str) -> io::Result<String> {
    let key_bytes = load_or_create_key()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));

    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(err)?;

    let mut combined = nonce_bytes.to_vec();
    combined.extend(ciphertext);
    Ok(STANDARD.encode(combined))
}

pub fn decrypt(encoded: &str) -> io::Result<String> {
    let key_bytes = load_or_create_key()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));

    let combined = STANDARD
        .decode(encoded)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if combined.len() < 12 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "ciphertext too short"));
    }
    let (nonce_bytes, ciphertext) = combined.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher.decrypt(nonce, ciphertext).map_err(err)?;
    String::from_utf8(plaintext).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Like `encrypt`, but returns an empty string for an empty input instead
/// of encrypting a zero-length secret — handy for "optional password"
/// fields (e.g. an SSH key-only login with no password) where we don't
/// want to churn out ciphertext for nothing.
pub fn encrypt_optional(plaintext: &str) -> io::Result<String> {
    if plaintext.is_empty() {
        return Ok(String::new());
    }
    encrypt(plaintext)
}

/// Inverse of `encrypt_optional`.
pub fn decrypt_optional(encoded: &str) -> io::Result<String> {
    if encoded.is_empty() {
        return Ok(String::new());
    }
    decrypt(encoded)
}
