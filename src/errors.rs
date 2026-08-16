use thiserror::Error;

#[derive(Error, Debug)]
pub enum CliError {
    #[error("{message}")]
    User {
        message: String,
        code: Option<String>,
    },

    #[error("HTTP {status}: {message}")]
    Http {
        status: u16,
        message: String,
        details: Option<serde_json::Value>,
    },

    #[error("Not authenticated. Run `ilert auth login` first.")]
    NotAuthenticated,

    /// The profile's stored credential belongs to a different environment than
    /// the one this invocation is pointed at. A distinct variant because a
    /// command that merely *accepts* authentication should carry on without it
    /// rather than fail — see `ResolvedConfig::resolve_credential_opt`.
    #[error("{message}")]
    CredentialEndpointMismatch { message: String },

    /// A destructive command was refused because nothing could confirm it.
    /// Carries the structured envelope from `crate::preview`, which the error
    /// path prints verbatim instead of the usual human-readable message.
    #[error("Confirmation required. Re-run with --yes to proceed.")]
    ConfirmationRequired { payload: Box<serde_json::Value> },

    /// A human was asked and said no. Same exit status as a refusal — the
    /// operation did not happen — but no envelope, because a person read the
    /// prompt and does not need the machine-readable version of it.
    #[error("Cancelled. Nothing was changed.")]
    Cancelled,
}

/// Exit status for a refused destructive command. Distinct from `1` so a
/// caller can tell "you didn't consent" apart from "it went wrong".
pub const EXIT_CONFIRMATION_REQUIRED: i32 = 2;
pub const EXIT_FAILURE: i32 = 1;

impl CliError {
    pub fn user(message: impl Into<String>) -> Self {
        Self::User {
            message: message.into(),
            code: None,
        }
    }

    pub fn confirmation_required(payload: serde_json::Value) -> Self {
        Self::ConfirmationRequired {
            payload: Box::new(payload),
        }
    }
}

/// The process exit code for a failed run.
pub fn exit_code(err: &anyhow::Error) -> i32 {
    match err.downcast_ref::<CliError>() {
        Some(CliError::ConfirmationRequired { .. }) | Some(CliError::Cancelled) => {
            EXIT_CONFIRMATION_REQUIRED
        }
        _ => EXIT_FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_refusal_exits_two() {
        let err: anyhow::Error = CliError::confirmation_required(serde_json::json!({})).into();
        assert_eq!(exit_code(&err), EXIT_CONFIRMATION_REQUIRED);
    }

    #[test]
    fn ordinary_failures_exit_one() {
        assert_eq!(exit_code(&CliError::user("nope").into()), EXIT_FAILURE);
        assert_eq!(exit_code(&CliError::NotAuthenticated.into()), EXIT_FAILURE);
        assert_eq!(
            exit_code(&anyhow::anyhow!("something unrelated")),
            EXIT_FAILURE
        );
    }
}
