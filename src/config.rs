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
    /// OAuth2 application to authenticate as. Only set on profiles pointing at
    /// a non-production environment, which register their own application —
    /// production is left unset so it tracks the binary's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_client_id: Option<String>,
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

/// An API key carrying a single invocation, and where it may be sent.
///
/// The two ways of supplying one are not equally deliberate. A key typed on the
/// command line arrives next to the endpoint it is meant for, so the pairing is
/// the operator's own. A key inherited from `ILERT_API_KEY` was exported for the
/// environment the shell (or the CI job) is set up for, and its holder never saw
/// the command line that later picks it up — so it stays bound to that
/// environment and a `--base-url` cannot redirect it.
#[derive(Debug, Clone)]
pub struct ExplicitApiKey {
    pub key: String,
    /// Normalized endpoint this key may be sent to; `None` when it may go
    /// wherever the command line says.
    pub bound_to: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// API key supplied explicitly via `--api-key` or `ILERT_API_KEY`.
    /// Highest precedence and never persisted to the keyring.
    pub explicit_api_key: Option<ExplicitApiKey>,
    pub base_url: String,
    /// The endpoint stored in the profile itself, before any flag or environment
    /// override. Used to place credentials that predate endpoint binding.
    pub profile_base_url: Option<String>,
    /// OAuth2 application this profile authenticates as. Always populated —
    /// [`crate::oauth::DEFAULT_CLIENT_ID`] when nothing overrides it.
    pub oauth_client_id: String,
    pub team_context: Option<String>,
    pub profile_name: String,
}

impl ResolvedConfig {
    /// The endpoint and application identity for this profile's OAuth calls.
    pub fn oauth(&self) -> crate::oauth::OauthConfig<'_> {
        crate::oauth::OauthConfig {
            base_url: &self.base_url,
            client_id: &self.oauth_client_id,
        }
    }

    /// The environment a stored credential belongs to, normalized.
    ///
    /// Credentials written before endpoint binding existed carry nothing, so
    /// they fall back to the profile's own endpoint and then to production —
    /// which is where such a credential must have come from, since login has
    /// always persisted the base URL it was given. The fallbacks deliberately
    /// never consult [`Self::base_url`]: that is the value an override can
    /// control, and trusting it would answer the question with the question.
    pub fn credential_endpoint(&self, cred: &Credential) -> String {
        let source = cred
            .base_url()
            .or(self.profile_base_url.as_deref())
            .unwrap_or(DEFAULT_BASE_URL);
        normalize_base_url(source)
    }

    /// Refuse to hand a stored credential to an endpoint it was not issued for.
    ///
    /// `--base-url` (and `ILERT_BASE_URL`) can point a command anywhere, but a
    /// profile's credential belongs to exactly one environment: sending a
    /// production token to another host discloses it to an operator who should
    /// never have held it. Environments get their own profile; an endpoint that
    /// genuinely needs an ad-hoc credential gets it passed in explicitly.
    pub fn ensure_credential_matches_endpoint(&self, cred: &Credential) -> Result<()> {
        let issued_for = self.credential_endpoint(cred);
        let target = normalize_base_url(&self.base_url);
        if issued_for == target {
            return Ok(());
        }
        Err(CliError::CredentialEndpointMismatch {
            message: format!(
                "Refusing to send profile '{profile}' credentials to {target} — they were issued for {issued_for}.\n\
                 Log in to a separate profile for that environment:\n  \
                 ilert --profile <name> auth login --base-url {target}\n\
                 Then select it with --profile (or ILERT_PROFILE). To authenticate a single \
                 invocation instead, pass --api-key on the command line.",
                profile = self.profile_name,
            ),
        }
        .into())
    }

    /// Refuse to send an inherited API key somewhere its environment does not
    /// point. See [`ExplicitApiKey`] for why an exported key is treated as
    /// belonging to an environment while a flag key is not.
    pub fn ensure_explicit_key_matches_endpoint(&self, key: &ExplicitApiKey) -> Result<()> {
        let Some(bound_to) = key.bound_to.as_deref() else {
            return Ok(());
        };
        let target = normalize_base_url(&self.base_url);
        if bound_to == target {
            return Ok(());
        }
        Err(CliError::CredentialEndpointMismatch {
            message: format!(
                "Refusing to send the API key from ILERT_API_KEY to {target} — this environment \
                 points at {bound_to}.\n\
                 Pass --api-key on the command line to send a key to an endpoint of your own \
                 choosing, or set ILERT_BASE_URL={target} if that is where this key belongs.",
            ),
        }
        .into())
    }

    /// Resolve the final `Authorization: Bearer` value, silently refreshing (and
    /// re-persisting) an OAuth credential when it is at/near expiry.
    ///
    /// Precedence: explicit api key (flag/env) → stored Credential (keyring).
    /// There is no third source: `config.json` holds settings, never secrets.
    pub async fn resolve_credential(&self) -> Result<String> {
        // 1. Explicit api key (transient — never persisted).
        if let Some(explicit) = &self.explicit_api_key {
            ensure_secure_base_url(&self.base_url)?;
            self.ensure_explicit_key_matches_endpoint(explicit)?;
            return Ok(explicit.key.clone());
        }

        // 2. Stored credential for this profile.
        if let Some(cred) = crate::secret_store::retrieve(&self.profile_name)? {
            // Before anything leaves the process — including the refresh
            // exchange below, which is itself a request carrying a secret.
            self.ensure_credential_matches_endpoint(&cred)?;
            return match cred {
                Credential::ApiKey { ref key, .. } => {
                    ensure_secure_base_url(&self.base_url)?;
                    Ok(key.clone())
                }
                Credential::OAuth { .. } => self.bearer_from_oauth(cred).await,
            };
        }

        Err(CliError::NotAuthenticated.into())
    }

    /// Like [`resolve_credential`], but returns `None` instead of erroring when
    /// no credential is available (for commands where auth is optional).
    ///
    /// A credential belonging to another environment counts as unavailable: an
    /// event send authenticates with its integration key, so the right outcome
    /// is to leave the profile's token at home, not to fail the send.
    pub async fn resolve_credential_opt(&self) -> Result<Option<String>> {
        match self.resolve_credential().await {
            Ok(value) => Ok(Some(value)),
            Err(e) => match e.downcast_ref::<CliError>() {
                Some(CliError::NotAuthenticated | CliError::CredentialEndpointMismatch { .. }) => {
                    Ok(None)
                }
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
        let resp = crate::oauth::refresh(self.oauth(), refresh_token).await?;
        let mut refreshed = resp.into_credential(&self.base_url);
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

/// A canonical form of a base URL, for binding a credential to an environment
/// and for comparing one against the endpoint a command is about to use.
///
/// Scheme and host are case-insensitive and a default port is not part of an
/// endpoint's identity, so those are folded. **The path is not**: an instance can
/// live under a path prefix, which makes `https://host/a` and `https://host/b`
/// different environments that must not share credentials.
pub fn normalize_base_url(base_url: &str) -> String {
    let Ok(url) = url::Url::parse(base_url) else {
        // Not a URL at all. It will be rejected before anything is sent; return
        // something stable so the comparison stays meaningful until then.
        return base_url.trim().trim_end_matches('/').to_ascii_lowercase();
    };
    let mut out = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    // `port` is `None` when the port is the scheme's default, which is what
    // makes `https://host` and `https://host:443` the same environment.
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    out.push_str(url.path().trim_end_matches('/'));
    out
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
        oauth_client_id_override: Option<&str>,
        team_context_override: Option<&str>,
    ) -> ResolvedConfig {
        // The profile the environment selects on its own, and the one this
        // invocation actually runs as. They are resolved separately because an
        // exported API key belongs to the former: `--profile` is part of the
        // command line, so letting it pick the profile an inherited key is bound
        // to would let the command line choose its own binding — and
        // `ILERT_API_KEY=production-key ilert --profile staging ...` would send
        // production's key to staging.
        let ambient_profile_name = std::env::var("ILERT_PROFILE")
            .ok()
            .or_else(|| self.config.default_profile.clone())
            .unwrap_or_else(|| "default".to_string());

        let profile_name = profile_override
            .map(String::from)
            .unwrap_or_else(|| ambient_profile_name.clone());

        let profile = self.config.profiles.get(&profile_name);

        // Where this environment points on its own, before the command line has
        // a say. An exported API key belongs here, not wherever a flag aims.
        let ambient_base_url = std::env::var("ILERT_BASE_URL")
            .ok()
            .or_else(|| {
                self.config
                    .profiles
                    .get(&ambient_profile_name)
                    .and_then(|p| p.base_url.clone())
            })
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let explicit_api_key = match api_key_override {
            Some(key) => Some(ExplicitApiKey {
                key: key.to_string(),
                bound_to: None,
            }),
            None => std::env::var("ILERT_API_KEY")
                .ok()
                .map(|key| ExplicitApiKey {
                    key,
                    bound_to: Some(normalize_base_url(&ambient_base_url)),
                }),
        };

        // The endpoint this invocation actually talks to, resolved down the full
        // chain — including the profile the command line selected. Where it
        // differs from the ambient one, an inherited key is refused above.
        let base_url = base_url_override
            .map(String::from)
            .or_else(|| std::env::var("ILERT_BASE_URL").ok())
            .or_else(|| profile.and_then(|p| p.base_url.clone()))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        // Resolved on the same chain as the base URL, and for the same reason:
        // every environment registers its own OAuth application, so the pair has
        // to move together. Only the production id is compiled in.
        let oauth_client_id = oauth_client_id_override
            .map(String::from)
            .or_else(|| std::env::var("ILERT_OAUTH_CLIENT_ID").ok())
            .or_else(|| profile.and_then(|p| p.oauth_client_id.clone()))
            .unwrap_or_else(|| crate::oauth::DEFAULT_CLIENT_ID.to_string());

        let team_context = team_context_override
            .map(String::from)
            .or_else(|| std::env::var("ILERT_TEAM_CONTEXT").ok())
            .or_else(|| profile.and_then(|p| p.team_context.clone()));

        ResolvedConfig {
            explicit_api_key,
            base_url,
            profile_base_url: profile.and_then(|p| p.base_url.clone()),
            oauth_client_id,
            team_context,
            profile_name,
        }
    }

    /// The stored settings for a profile, if it has any.
    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.config.profiles.get(name)
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
    use super::{check_base_url_scheme, normalize_base_url};

    #[test]
    fn normalization_folds_what_does_not_identify_an_environment() {
        let canonical = "https://api.ilert.com";
        for equivalent in [
            "https://api.ilert.com",
            "https://api.ilert.com/",
            "https://API.ILERT.com",
            "HTTPS://api.ilert.com",
            "https://api.ilert.com:443",
        ] {
            assert_eq!(normalize_base_url(equivalent), canonical, "{equivalent}");
        }
    }

    #[test]
    fn normalization_keeps_what_does_identify_one() {
        // A path prefix separates two environments on one host, so folding it
        // would let either one's credentials reach the other.
        assert_ne!(
            normalize_base_url("https://gateway.example.com/tenant-a"),
            normalize_base_url("https://gateway.example.com/tenant-b")
        );
        // As do host, scheme and a non-default port.
        assert_ne!(
            normalize_base_url("https://api.ilert.com"),
            normalize_base_url("https://api.ilert.dev")
        );
        assert_ne!(
            normalize_base_url("https://api.ilert.com"),
            normalize_base_url("https://api.ilert.com:8443")
        );
        assert_ne!(
            normalize_base_url("https://api.ilert.com"),
            normalize_base_url("http://api.ilert.com")
        );
    }

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
