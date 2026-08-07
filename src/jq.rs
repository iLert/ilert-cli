//! `--jq` — filter JSON output through the `jq` binary.
//!
//! Shelling out rather than embedding a jq implementation keeps the expression
//! language exactly the one users already know, at the cost of a dependency we
//! have to report clearly when it is missing.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use serde_json::Value;

use crate::errors::CliError;

/// Run `value` through `jq <expression>` and return jq's stdout verbatim.
pub fn filter(expression: &str, value: &Value) -> Result<String> {
    let input = serde_json::to_string(value)?;

    let mut child = Command::new("jq")
        .arg(expression)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CliError::user(
                    "--jq requires the 'jq' binary, which was not found on PATH. \
                     Install it (https://jqlang.github.io/jq/) or drop --jq and filter with --fields.",
                )
            } else {
                CliError::user(format!("Failed to run jq: {e}"))
            }
        })?;

    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(input.as_bytes())
        .map_err(|e| CliError::user(format!("Failed to write to jq: {e}")))?;

    let output = child
        .wait_with_output()
        .map_err(|e| CliError::user(format!("Failed to read jq output: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = match stderr.trim() {
            "" => "no output",
            message => message,
        };
        return Err(CliError::user(format!("jq failed: {detail}")).into());
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jq_available() -> bool {
        Command::new("jq")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    #[test]
    fn filters_a_json_document() {
        if !jq_available() {
            return;
        }
        let out = filter(
            ".[].summary",
            &json!([{"summary": "one"}, {"summary": "two"}]),
        )
        .unwrap();
        assert_eq!(out.trim(), "\"one\"\n\"two\"");
    }

    #[test]
    fn a_bad_expression_is_an_error_not_a_panic() {
        if !jq_available() {
            return;
        }
        let err = filter("this is not jq", &json!({})).unwrap_err();
        assert!(err.to_string().contains("jq failed"));
    }
}
