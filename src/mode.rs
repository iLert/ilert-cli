//! Which kind of caller is driving the CLI?
//!
//! We used to branch on a single binary signal — `stdin().is_terminal()` — and
//! treat everything non-TTY as "scripted, just proceed". That silently skipped
//! the delete confirmation in exactly the contexts where an accidental delete is
//! most likely and least visible.
//!
//! Detection precedence:
//!
//! 1. `ILERT_CLI_MODE=interactive|ci|agent`
//! 2. A known coding-agent environment marker
//! 3. A known CI environment marker
//! 4. `interactive` when stdin is a TTY
//! 5. `ci` — the conservative fallback for any other non-TTY invocation
//!
//! Step 5 is the important one: marker detection adds useful provenance, but
//! safety must never depend on us recognising every automation product.

use std::io::IsTerminal;

use anyhow::Result;

use crate::errors::CliError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliMode {
    Interactive,
    Ci,
    Agent,
}

impl CliMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Ci => "ci",
            Self::Agent => "agent",
        }
    }

    /// Only an interactive caller can answer a prompt.
    pub fn can_prompt(self) -> bool {
        matches!(self, Self::Interactive)
    }
}

/// The resolved mode plus what decided it, for `--log-level debug`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    pub mode: CliMode,
    pub source: &'static str,
}

pub const MODE_VAR: &str = "ILERT_CLI_MODE";

/// Environment variables that mean "a coding agent is driving this".
/// Presence is the signal; these are set by the agent runtime, not by users.
const AGENT_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE",
    "CLAUDE_CODE_IS_COWORK",
    "CURSOR_TRACE_ID",
    "CURSOR_AGENT",
    "CODEX_SANDBOX",
    "CODEX_CI",
    "CODEX_THREAD_ID",
    "AIDER",
    "AMP_CURRENT_THREAD_ID",
    "WINDSURF",
    "WINDSURF_AGENT",
    "CODEIUM_ENV",
    "CLINE_ACTIVE",
    "GEMINI_CLI",
    "OPENCODE",
    "VSCODE_AGENT",
    "COPILOT_AGENT",
    "COPILOT_CLI",
    "GITHUB_COPILOT",
    "COPILOT_MODEL",
    "GOOSE_TERMINAL",
    "ANTIGRAVITY_AGENT",
    "AUGMENT_AGENT",
    "KIRO_AGENT_PATH",
    "TRAE_AI_SHELL_ID",
    "ANDROID_STUDIO_AGENT",
];

/// Generic agent markers. Unlike the product-specific ones above these are
/// commonly set by hand, so an explicit falsey value has to switch them off.
const GENERIC_AGENT_MARKERS: &[&str] = &["AI_AGENT", "AGENT"];

/// Presence-based CI markers, plus `CI` itself which is handled as truthy.
const CI_MARKERS: &[&str] = &[
    "GITHUB_ACTIONS",
    "GITLAB_CI",
    "BUILDKITE",
    "CIRCLECI",
    "JENKINS_URL",
    "TEAMCITY_VERSION",
    "TF_BUILD",
    "BITBUCKET_BUILD_NUMBER",
    "DRONE",
    "APPVEYOR",
];

const FALSEY: &[&str] = &["0", "false", "no", "off", ""];

/// Detect the mode from the real process environment.
pub fn detect() -> Result<CliMode> {
    Ok(detect_full()?.mode)
}

pub fn detect_full() -> Result<Detection> {
    resolve(
        &|key: &str| std::env::var(key).ok(),
        std::io::stdin().is_terminal(),
    )
}

/// Pure resolution, so tests don't have to mutate process-global env state.
pub fn resolve(env: &dyn Fn(&str) -> Option<String>, stdin_is_tty: bool) -> Result<Detection> {
    // 1. Explicit override. An unrecognised value is a usage error rather than a
    // silent fallback — a typo here would quietly re-open the hole this module
    // exists to close.
    if let Some(raw) = env(MODE_VAR) {
        let value = raw.trim().to_ascii_lowercase();
        if !value.is_empty() {
            let mode = match value.as_str() {
                "interactive" => CliMode::Interactive,
                "ci" => CliMode::Ci,
                "agent" => CliMode::Agent,
                _ => {
                    return Err(CliError::user(format!(
                        "Invalid {MODE_VAR} value '{raw}'. Expected one of: interactive, ci, agent."
                    ))
                    .into());
                }
            };
            return Ok(Detection {
                mode,
                source: MODE_VAR,
            });
        }
    }

    // 2. Agent markers win over CI markers: an agent running inside CI is still
    // an agent, and the agent envelope is the more useful response for it.
    for marker in AGENT_MARKERS {
        if env(marker).is_some() {
            return Ok(Detection {
                mode: CliMode::Agent,
                source: marker,
            });
        }
    }
    for marker in GENERIC_AGENT_MARKERS {
        if is_truthy(env(marker).as_deref()) {
            return Ok(Detection {
                mode: CliMode::Agent,
                source: marker,
            });
        }
    }

    // 3. CI markers.
    for marker in CI_MARKERS {
        if env(marker).is_some() {
            return Ok(Detection {
                mode: CliMode::Ci,
                source: marker,
            });
        }
    }
    if is_truthy(env("CI").as_deref()) {
        return Ok(Detection {
            mode: CliMode::Ci,
            source: "CI",
        });
    }

    // 4/5. Fall back on the terminal, defaulting to the cautious side.
    Ok(if stdin_is_tty {
        Detection {
            mode: CliMode::Interactive,
            source: "tty",
        }
    } else {
        Detection {
            mode: CliMode::Ci,
            source: "no-tty",
        }
    })
}

fn is_truthy(value: Option<&str>) -> bool {
    match value {
        Some(v) => !FALSEY.contains(&v.trim().to_ascii_lowercase().as_str()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn mode(pairs: &[(&str, &str)], tty: bool) -> CliMode {
        resolve(&env_of(pairs), tty)
            .expect("resolution failed")
            .mode
    }

    #[test]
    fn tty_without_markers_is_interactive() {
        assert_eq!(mode(&[], true), CliMode::Interactive);
    }

    #[test]
    fn non_tty_without_markers_falls_back_to_ci() {
        // The whole point: an unrecognised pipe or automation runner must not
        // be treated as "just proceed".
        assert_eq!(mode(&[], false), CliMode::Ci);
    }

    #[test]
    fn agent_markers_win_even_on_a_tty() {
        assert_eq!(mode(&[("CLAUDECODE", "1")], true), CliMode::Agent);
        assert_eq!(mode(&[("CURSOR_AGENT", "1")], false), CliMode::Agent);
        assert_eq!(mode(&[("AIDER", "1")], true), CliMode::Agent);
    }

    #[test]
    fn agent_markers_win_over_ci_markers() {
        assert_eq!(
            mode(&[("GITHUB_ACTIONS", "true"), ("CLAUDECODE", "1")], false),
            CliMode::Agent
        );
    }

    #[test]
    fn generic_agent_markers_respect_falsey_values() {
        assert_eq!(mode(&[("AI_AGENT", "claude-code")], true), CliMode::Agent);
        assert_eq!(mode(&[("AI_AGENT", "1")], true), CliMode::Agent);
        assert_eq!(mode(&[("AI_AGENT", "0")], true), CliMode::Interactive);
        assert_eq!(mode(&[("AGENT", "false")], true), CliMode::Interactive);
        assert_eq!(mode(&[("AGENT", "")], true), CliMode::Interactive);
    }

    #[test]
    fn ci_markers_are_detected() {
        assert_eq!(mode(&[("GITHUB_ACTIONS", "true")], true), CliMode::Ci);
        assert_eq!(mode(&[("GITLAB_CI", "true")], true), CliMode::Ci);
        assert_eq!(mode(&[("CI", "true")], true), CliMode::Ci);
        assert_eq!(mode(&[("CI", "0")], true), CliMode::Interactive);
    }

    #[test]
    fn explicit_mode_overrides_everything() {
        assert_eq!(
            mode(&[(MODE_VAR, "interactive"), ("CLAUDECODE", "1")], false),
            CliMode::Interactive
        );
        assert_eq!(mode(&[(MODE_VAR, "AGENT")], true), CliMode::Agent);
        assert_eq!(mode(&[(MODE_VAR, " ci ")], true), CliMode::Ci);
    }

    #[test]
    fn invalid_explicit_mode_is_a_usage_error() {
        let err = resolve(&env_of(&[(MODE_VAR, "yolo")]), true).unwrap_err();
        assert!(err.to_string().contains("Invalid ILERT_CLI_MODE"));
    }

    #[test]
    fn empty_explicit_mode_falls_through() {
        assert_eq!(mode(&[(MODE_VAR, "")], true), CliMode::Interactive);
    }

    #[test]
    fn only_interactive_can_prompt() {
        assert!(CliMode::Interactive.can_prompt());
        assert!(!CliMode::Ci.can_prompt());
        assert!(!CliMode::Agent.can_prompt());
    }

    #[test]
    fn detection_reports_its_source() {
        let d = resolve(&env_of(&[("OPENCODE", "1")]), false).unwrap();
        assert_eq!(d.source, "OPENCODE");
        assert_eq!(d.mode, CliMode::Agent);
    }
}
