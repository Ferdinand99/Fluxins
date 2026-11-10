//! Keeps the session token in the OS keychain (macOS Keychain, Windows Credential
//! Manager, Secret Service on Linux). The token is never written to a file and never
//! logged. The only thing on disk is the address of the last instance, which is not secret.

use anyhow::{Context, Result};

const SERVICE: &str = "inlet";

fn entry(instance: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, instance).context("open the keychain entry")
}

pub fn save_token(instance: &str, token: &str) -> Result<()> {
    entry(instance)?.set_password(token).context("store the token")
}

pub fn load_token(instance: &str) -> Result<Option<String>> {
    match entry(instance)?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(err).context("read the token"),
    }
}

pub fn delete_token(instance: &str) -> Result<()> {
    match entry(instance)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(err).context("delete the token"),
    }
}

fn state_file() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("inlet").join("last-instance"))
}

pub fn last_instance() -> Option<String> {
    let s = std::fs::read_to_string(state_file()?).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

pub fn set_last_instance(instance: &str) {
    if let Some(path) = state_file() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, instance);
    }
}
