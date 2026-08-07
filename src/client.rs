//! Shared reqwest client construction.
//!
//! Every outbound request — API calls, spec fetches, OAuth token exchange,
//! heartbeat pings and the update check — is built from [`builder`] so CLI
//! traffic identifies itself consistently in server logs. reqwest applies the
//! User-Agent per request, so retries carry it too.

/// Version of this binary, as declared in Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `ilert-cli/<version>`
pub fn user_agent() -> String {
    format!("ilert-cli/{VERSION}")
}

/// A client builder pre-loaded with our identifying header. Callers add their
/// own timeout and redirect policy on top.
pub fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().user_agent(user_agent())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_has_the_documented_shape() {
        assert_eq!(user_agent(), format!("ilert-cli/{VERSION}"));
    }

    #[test]
    fn user_agent_does_not_disclose_the_platform() {
        let ua = user_agent();
        assert!(!ua.contains(std::env::consts::OS), "{ua} leaks the OS");
        assert!(!ua.contains(std::env::consts::ARCH), "{ua} leaks the arch");
    }
}
