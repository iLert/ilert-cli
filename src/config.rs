use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::errors::CliError;
use crate::secret_store::Credential;

const DEFAULT_BASE_URL: &str = "https://api.ilert.com";

/// Non-secret, per-profile settings. Credentials never live here — they go to
/// the OS keyring via [`crate::secret_store`], or are passed in per-invocation
/// with `--api-key` / `ILERT_API_KEY`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Profile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// API key supplied explicitly via `--api-key` or `ILERT_API_KEY`.
    /// Highest precedence and never persisted to the keyring.
    pub explicit_api_key: Option<String>,
    pub base_url: String,
    pub team_context: Option<String>,
    pub profile_name: String,
}

impl ResolvedConfig {
    /// Resolve the final `Authorization: Bearer` value, silently refreshing (and
    /// re-persisting) an OAuth credential when it is at/near expiry.
    ///
    /// Precedence: explicit api key (flag/env) → stored Credential (keyring).
    /// There is no third source: `config.json` holds settings, never secrets.
    pub async fn resolve_credential(&self) -> Result<String> {
        // 1. Explicit api key (transient — never persisted).
        if let Some(key) = &self.explicit_api_key {
            ensure_secure_base_url(&self.base_url)?;
            return Ok(key.clone());
        }

        // 2. Stored credential for this profile.
        if let Some(cred) = crate::secret_store::retrieve(&self.profile_name)? {
            return match cred {
                Credential::ApiKey { key } => {
                    ensure_secure_base_url(&self.base_url)?;
                    Ok(key)
                }
                Credential::OAuth { .. } => self.bearer_from_oauth(cred).await,
            };
        }

        Err(CliError::NotAuthenticated.into())
    }

    /// Like [`resolve_credential`], but returns `None` instead of erroring when
    /// no credential is available (for commands where auth is optional).
    pub async fn resolve_credential_opt(&self) -> Result<Option<String>> {
        match self.resolve_credential().await {
            Ok(value) => Ok(Some(value)),
            Err(e) => match e.downcast_ref::<CliError>() {
                Some(CliError::NotAuthenticated) => Ok(None),
                _ => Err(e),
            },
        }
    }

    async fn bearer_from_oauth(&self, cred: Credential) -> Result<String> {
        // Never hand out (or refresh) a token destined for a cleartext endpoint.
        ensure_secure_base_url(&self.base_url)?;
        if !cred.needs_refresh() {
            return Ok(cred.bearer_value().to_string());
        }
        let refresh_token = cred.refresh_token().ok_or_else(|| {
            CliError::user(
                "OAuth session expired and no refresh token is stored. Run `ilert auth login` again.",
            )
        })?;
        let resp = crate::oauth::refresh(&self.base_url, refresh_token).await?;
        let mut refreshed = resp.into_credential();
        // Preserve the existing refresh token if the server didn't rotate it.
        if let Credential::OAuth {
            refresh_token: rt, ..
        } = &mut refreshed
            && rt.is_none()
        {
            *rt = cred.refresh_token().map(str::to_string);
        }
        crate::secret_store::store(&self.profile_name, &refreshed)?;
        Ok(refreshed.bearer_value().to_string())
    }
}

/// Reject sending credentials to a non-HTTPS endpoint. Cleartext HTTP is only
/// allowed for loopback hosts (local development / tests). Set
/// `ILERT_ALLOW_INSECURE_HTTP=1` to override (strongly discouraged).
pub fn ensure_secure_base_url(base_url: &str) -> Result<()> {
    let allow_insecure = std::env::var("ILERT_ALLOW_INSECURE_HTTP")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    check_base_url_scheme(base_url, allow_insecure)
}

/// Pure scheme/host check behind [`ensure_secure_base_url`] (no env access, for tests).
fn check_base_url_scheme(base_url: &str, allow_insecure: bool) -> Result<()> {
    if allow_insecure {
        return Ok(());
    }
    let url = url::Url::parse(base_url)
        .map_err(|_| CliError::user(format!("Invalid base URL: {base_url}")))?;
    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            let is_loopback = match url.host() {
                Some(url::Host::Domain(d)) => d == "localhost" || d.ends_with(".localhost"),
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                None => false,
            };
            if is_loopback {
                Ok(())
            } else {
                let host = url.host_str().unwrap_or("");
                Err(CliError::user(format!(
                    "Refusing to send credentials over cleartext HTTP to '{host}'. \
                     Use an https:// base URL, or set ILERT_ALLOW_INSECURE_HTTP=1 to override."
                ))
                .into())
            }
        }
        other => Err(CliError::user(format!(
            "Unsupported URL scheme '{other}' in base URL: {base_url}"
        ))
        .into()),
    }
}

pub struct ConfigManager {
    config_path: PathBuf,
    config: ConfigFile,
}

impl ConfigManager {
    pub fn load() -> Result<Self> {
        let config_path = Self::config_file_path()?;

        let config = if config_path.exists() {
            let content =
                std::fs::read_to_string(&config_path).context("Failed to read config file")?;
            serde_json::from_str(&content).context("Failed to parse config file")?
        } else {
            ConfigFile::default()
        };

        Ok(Self {
            config_path,
            config,
        })
    }

    pub fn resolve(
        &self,
        profile_override: Option<&str>,
        api_key_override: Option<&str>,
        base_url_override: Option<&str>,
        team_context_override: Option<&str>,
    ) -> ResolvedConfig {
        let profile_name = profile_override
            .map(String::from)
            .or_else(|| std::env::var("ILERT_PROFILE").ok())
            .or_else(|| self.config.default_profile.clone())
            .unwrap_or_else(|| "default".to_string());

        let profile = self.config.profiles.get(&profile_name);

        let explicit_api_key = api_key_override
            .map(String::from)
            .or_else(|| std::env::var("ILERT_API_KEY").ok());

        let base_url = base_url_override
            .map(String::from)
            .or_else(|| std::env::var("ILERT_BASE_URL").ok())
            .or_else(|| profile.and_then(|p| p.base_url.clone()))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let team_context = team_context_override
            .map(String::from)
            .or_else(|| std::env::var("ILERT_TEAM_CONTEXT").ok())
            .or_else(|| profile.and_then(|p| p.team_context.clone()));

        ResolvedConfig {
            explicit_api_key,
            base_url,
            team_context,
            profile_name,
        }
    }

    pub fn save_profile(&mut self, name: &str, profile: Profile) -> Result<()> {
        self.config.profiles.insert(name.to_string(), profile);
        self.save()
    }

    pub fn set_default_profile(&mut self, name: &str) -> Result<()> {
        self.config.default_profile = Some(name.to_string());
        self.save()
    }

    pub fn list_profiles(&self) -> Vec<(&str, bool)> {
        let default = self.config.default_profile.as_deref().unwrap_or("default");
        self.config
            .profiles
            .keys()
            .map(|name| (name.as_str(), name == default))
            .collect()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent).context("Failed to create config directory")?;
        }
        let content =
            serde_json::to_string_pretty(&self.config).context("Failed to serialize config")?;
        std::fs::write(&self.config_path, content).context("Failed to write config file")?;
        Ok(())
    }

    pub fn config_file_path() -> Result<PathBuf> {
        // Respect XDG env vars (essential for testing and Linux)
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(xdg).join("ilert").join("config.json"));
        }
        let dirs = ProjectDirs::from("com", "ilert", "ilert-cli")
            .context("Could not determine config directory")?;
        Ok(dirs.config_dir().join("config.json"))
    }

    pub fn cache_dir() -> Result<PathBuf> {
        if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
            return Ok(PathBuf::from(xdg).join("ilert"));
        }
        let dirs = ProjectDirs::from("com", "ilert", "ilert-cli")
            .context("Could not determine cache directory")?;
        Ok(dirs.cache_dir().to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::check_base_url_scheme;

    #[test]
    fn https_is_allowed() {
        assert!(check_base_url_scheme("https://api.ilert.com", false).is_ok());
        assert!(check_base_url_scheme("https://api.ilert.com/", false).is_ok());
    }

    #[test]
    fn loopback_http_is_allowed() {
        assert!(check_base_url_scheme("http://127.0.0.1:8080", false).is_ok());
        assert!(check_base_url_scheme("http://localhost:3000", false).is_ok());
        assert!(check_base_url_scheme("http://[::1]:9000", false).is_ok());
    }

    #[test]
    fn remote_http_is_refused() {
        let err = check_base_url_scheme("http://api.ilert.com", false).unwrap_err();
        assert!(err.to_string().contains("cleartext"));
        assert!(check_base_url_scheme("http://10.0.0.5", false).is_err());
    }

    #[test]
    fn override_allows_remote_http() {
        assert!(check_base_url_scheme("http://api.ilert.com", true).is_ok());
    }

    #[test]
    fn non_http_scheme_is_refused() {
        assert!(check_base_url_scheme("ftp://api.ilert.com", false).is_err());
        assert!(check_base_url_scheme("not a url", false).is_err());
    }
}
