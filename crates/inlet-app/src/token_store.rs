// SPDX-License-Identifier: AGPL-3.0-or-later
//! Keeps the session token in the OS keychain (macOS Keychain, Windows Credential
//! Manager, Secret Service on Linux). The token is never written to a file and never
//! logged. On disk are only the address of the last instance and the installation ID,
//! neither of which is secret.
//!
//! Every function here blocks (keychain prompts, D-Bus, file IO). Call them from
//! `tokio::task::spawn_blocking` or a plain thread, never from async code directly.

use anyhow::{Context, Result};
use zeroize::Zeroizing;

const SERVICE: &str = "inlet";

fn entry(instance: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, instance).context("open the keychain entry")
}

/// What the keychain entry holds: the token and the hosts it was issued for, as one
/// JSON value. Older entries hold only the bare token.
#[derive(serde::Serialize, serde::Deserialize)]
struct Blob {
    token: String,
    pins: Option<inlet_protocol::EndpointPins>,
    /// The token was pasted (it belongs to another client): never sign it out on the
    /// server, only forget it. Entries written before this field count as not pasted.
    #[serde(default)]
    pasted: bool,
}

/// A stored session: the token (wiped from memory on drop) and its endpoint pins.
pub struct StoredSession {
    pub token: inlet_protocol::Token,
    /// `None` for an entry written before pinning existed.
    pub pins: Option<inlet_protocol::EndpointPins>,
    /// See [`Blob::pasted`].
    pub pasted: bool,
}

fn encode(
    token: &inlet_protocol::Token,
    pins: &inlet_protocol::EndpointPins,
    pasted: bool,
) -> Result<Zeroizing<String>> {
    let blob = Blob {
        token: token.expose_secret().to_string(),
        pins: Some(pins.clone()),
        pasted,
    };
    let json = Zeroizing::new(serde_json::to_string(&blob).context("encode the session")?);
    drop(Zeroizing::new(blob.token));
    Ok(json)
}

fn decode(raw: &str) -> StoredSession {
    match serde_json::from_str::<Blob>(raw) {
        Ok(blob) => StoredSession {
            pins: blob.pins,
            pasted: blob.pasted,
            token: inlet_protocol::Token::new(blob.token),
        },
        Err(_) => StoredSession {
            token: inlet_protocol::Token::new(raw.trim().to_string()),
            pins: None,
            pasted: false,
        },
    }
}

pub fn save_session(
    instance: &str,
    token: &inlet_protocol::Token,
    pins: &inlet_protocol::EndpointPins,
    pasted: bool,
) -> Result<()> {
    let json = encode(token, pins, pasted)?;
    entry(instance)?
        .set_password(&json)
        .context("store the token")
}

pub fn load_session(instance: &str) -> Result<Option<StoredSession>> {
    let raw = match entry(instance)?.get_password() {
        Ok(raw) => Zeroizing::new(raw),
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(err) => return Err(err).context("read the token"),
    };
    Ok(Some(decode(&raw)))
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

fn installation_id_file() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("inlet").join("installation-id"))
}

/// The client description sent to the instance, with an installation ID that stays the
/// same between starts (it identifies this install in the account's session list).
pub fn client_info() -> inlet_protocol::ClientInfo {
    let mut info = inlet_protocol::ClientInfo::default();
    let Some(path) = installation_id_file() else {
        return info;
    };
    match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => info.installation_id = id.trim().to_string(),
        _ => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, &info.installation_id);
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pins() -> inlet_protocol::EndpointPins {
        inlet_protocol::EndpointPins {
            api: "https://fluxer.example".into(),
            gateway: "wss://fluxer.example".into(),
        }
    }

    #[test]
    fn pasted_flag_round_trips() {
        let token = inlet_protocol::Token::new("secret");
        for pasted in [true, false] {
            let stored = decode(&encode(&token, &pins(), pasted).unwrap());
            assert_eq!(stored.pasted, pasted);
            assert_eq!(stored.token.expose_secret(), "secret");
            assert_eq!(stored.pins, Some(pins()));
        }
    }

    #[test]
    fn old_entries_count_as_not_pasted() {
        let old = r#"{"token":"secret","pins":{"api":"https://fluxer.example","gateway":"wss://fluxer.example"}}"#;
        let stored = decode(old);
        assert!(!stored.pasted);
        assert_eq!(stored.pins, Some(pins()));
        let bare = decode("bare-token");
        assert!(!bare.pasted);
        assert!(bare.pins.is_none());
    }
}
