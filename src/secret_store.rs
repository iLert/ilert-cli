//! Credential storage.
//!
//! Credentials are persisted as a single JSON blob per profile in the OS keyring
//! (service = `"ilert"`, account = profile name). A released binary has no other
//! backend: there is no configuration, and no environment variable, that moves a
//! stored credential to disk in cleartext.
//!
//! Debug builds additionally honor `ILERT_SECRET_FILE`, which points at a plain
//! JSON file used in place of the keyring. That is a test seam — it keeps e2e
//! runs isolated from the developer's real keyring — and it is compiled out of
//! release builds, mirroring the `ILERT_OAUTH_TEST_CODE` seam in [`crate::oauth`].
//! Headless and CI environments authenticate with a transient `ILERT_API_KEY`
//! instead, which is never persisted anywhere.

#[cfg(debug_assertions)]
use std::collections::HashMap;
#[cfg(debug_assertions)]
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Keyring service name. The account is the profile name.
const SERVICE: &str = "ilert";

/// Refresh an OAuth access token once it is within this many seconds of expiry.
const REFRESH_LEEWAY_SECS: i64 = 60;

/// A stored credential — either a raw API key or a set of OAuth2 tokens.
///
/// Every credential carries the environment it was issued for. A token minted by
/// one ilert instance is worthless to another and must never be offered to it, so
/// the binding travels with the secret rather than with the profile settings that
/// a flag or an environment variable can override — see
/// [`crate::config::ResolvedConfig::ensure_credential_matches_endpoint`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Credential {
    #[serde(rename = "api_key")]
    ApiKey {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
    },
    #[serde(rename = "oauth")]
    OAuth {
        access_token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
        expires_at: DateTime<Utc>,
        token_type: String,
        #[serde(default)]
        scopes: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
    },
}

impl Credential {
    /// The value to send in the `Authorization: Bearer` header.
    pub fn bearer_value(&self) -> &str {
        match self {
            Credential::ApiKey { key, .. } => key,
            Credential::OAuth { access_token, .. } => access_token,
        }
    }

    /// The normalized base URL this credential was issued for.
    ///
    /// `None` only for credentials written before the binding existed; callers
    /// resolve those against the profile instead of trusting the request.
    pub fn base_url(&self) -> Option<&str> {
        match self {
            Credential::ApiKey { base_url, .. } | Credential::OAuth { base_url, .. } => {
                base_url.as_deref()
            }
        }
    }

    /// True when this is an OAuth credential whose access token is expired or
    /// within the refresh leeway window. API keys never need refreshing.
    pub fn needs_refresh(&self) -> bool {
        match self {
            Credential::OAuth { expires_at, .. } => {
                *expires_at - Utc::now() < Duration::seconds(REFRESH_LEEWAY_SECS)
            }
            Credential::ApiKey { .. } => false,
        }
    }

    /// Record the environment this credential belongs to. Called on every write,
    /// so a credential refreshed by a newer binary picks up a binding it was
    /// stored without.
    pub fn bind_to(&mut self, endpoint: &str) {
        match self {
            Credential::ApiKey { base_url, .. } | Credential::OAuth { base_url, .. } => {
                *base_url = Some(endpoint.to_string());
            }
        }
    }

    pub fn refresh_token(&self) -> Option<&str> {
        match self {
            Credential::OAuth { refresh_token, .. } => refresh_token.as_deref(),
            Credential::ApiKey { .. } => None,
        }
    }
}

/// Storage backend selected at runtime. In a release build this has exactly one
/// variant — the keyring — so there is nothing to select.
enum Backend {
    Keyring,
    #[cfg(debug_assertions)]
    File(PathBuf),
}

fn backend() -> Backend {
    // Test seam: a plaintext file instead of the keyring, so e2e runs stay
    // isolated and never touch (or prompt for) the real one. Compiled out of
    // release builds — a shipped binary cannot be talked into plaintext storage.
    #[cfg(debug_assertions)]
    {
        if let Ok(path) = std::env::var("ILERT_SECRET_FILE")
            && !path.is_empty()
        {
            return Backend::File(PathBuf::from(path));
        }
    }
    Backend::Keyring
}

/// Human-readable name of the active credential store, for display (e.g. on the
/// post-login page). Accurate per platform and backend — it never claims a
/// "keychain" where the keyring isn't in use.
pub fn storage_label() -> String {
    match backend() {
        Backend::Keyring => {
            if cfg!(target_os = "macos") {
                "your macOS Keychain"
            } else if cfg!(target_os = "windows") {
                "Windows Credential Manager"
            } else {
                "your system keyring"
            }
        }
        #[cfg(debug_assertions)]
        Backend::File(_) => "a local file on this machine",
    }
    .to_string()
}

/// Store (or replace) the credential for `account` (the profile name).
pub fn store(account: &str, cred: &Credential) -> Result<()> {
    let json = serde_json::to_string(cred).context("Failed to serialize credential")?;
    match backend() {
        Backend::Keyring => {
            let entry =
                keyring::Entry::new(SERVICE, account).context("Failed to open keyring entry")?;
            entry
                .set_password(&json)
                .context("Failed to write credential to keyring")?;
        }
        #[cfg(debug_assertions)]
        Backend::File(path) => {
            let mut map = read_file_map(&path)?;
            map.insert(account.to_string(), cred.clone());
            write_file_map(&path, &map)?;
        }
    }
    Ok(())
}

/// Retrieve the credential for `account`, or `None` if none is stored.
pub fn retrieve(account: &str) -> Result<Option<Credential>> {
    match backend() {
        Backend::Keyring => {
            let entry =
                keyring::Entry::new(SERVICE, account).context("Failed to open keyring entry")?;
            match entry.get_password() {
                Ok(json) => {
                    let cred =
                        serde_json::from_str(&json).context("Failed to parse stored credential")?;
                    Ok(Some(cred))
                }
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => {
                    Err(anyhow::Error::new(e).context("Failed to read credential from keyring"))
                }
            }
        }
        #[cfg(debug_assertions)]
        Backend::File(path) => {
            let map = read_file_map(&path)?;
            Ok(map.get(account).cloned())
        }
    }
}

/// Delete the credential for `account`. Succeeds even if none is stored.
pub fn delete(account: &str) -> Result<()> {
    match backend() {
        Backend::Keyring => {
            let entry =
                keyring::Entry::new(SERVICE, account).context("Failed to open keyring entry")?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => {
                    Err(anyhow::Error::new(e).context("Failed to delete credential from keyring"))
                }
            }
        }
        #[cfg(debug_assertions)]
        Backend::File(path) => {
            let mut map = read_file_map(&path)?;
            if map.remove(account).is_some() {
                write_file_map(&path, &map)?;
            }
            Ok(())
        }
    }
}

#[cfg(debug_assertions)]
fn read_file_map(path: &PathBuf) -> Result<HashMap<String, Credential>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = std::fs::read_to_string(path).context("Failed to read secret file")?;
    if content.trim().is_empty() {
        return Ok(HashMap::new());
    }
    serde_json::from_str(&content).context("Failed to parse secret file")
}

#[cfg(debug_assertions)]
fn write_file_map(path: &PathBuf, map: &HashMap<String, Credential>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create secret file directory")?;
    }
    let content = serde_json::to_string_pretty(map).context("Failed to serialize secret file")?;

    // Write to a temp file in the same directory, then atomically rename over
    // the target. This avoids a torn/corrupt secret file if we crash mid-write.
    let tmp = path.with_extension("tmp");

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // Create the temp file 0600 from the start — no window where secrets
        // are readable with default (0644) permissions.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .context("Failed to open temp secret file")?;
        f.write_all(content.as_bytes())
            .context("Failed to write secret file")?;
        f.sync_all().ok();
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp, &content).context("Failed to write secret file")?;
    }

    std::fs::rename(&tmp, path).context("Failed to persist secret file")?;
    Ok(())
}
