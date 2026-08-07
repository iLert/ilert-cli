//! The one envelope a caller has to parse.
//!
//! `--dry-run` and a refused destructive command emit the same JSON shape and
//! differ only in `status` and which stream they go to (stdout for a successful
//! preview, stderr for a refusal, because the operation did not happen).
//!
//! We deliberately do not return an exact `confirmCommand`. Reconstructing shell
//! syntax is platform-dependent and would echo values supplied through flags
//! such as `--api-key`. The caller already has its own invocation; all it needs
//! from us is the name of the flag that grants consent.

use serde_json::Value;

use crate::classification::Classification;

pub const CONFIRM_FLAG: &str = "--yes";

pub const STATUS_DRY_RUN: &str = "dry_run";
pub const STATUS_CONFIRMATION_REQUIRED: &str = "confirmation_required";

/// Header names whose values never appear in a preview, regardless of where
/// they came from.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "x-auth-token",
];

/// Body fields whose values never appear in a preview or in `--debug` output.
///
/// Matched as substrings of the *normalized* key (lowercased, with `_`, `-` and
/// spaces removed), so one entry covers `apiKey`, `api_key`, `API-KEY` and
/// `apikey` alike. Substring matching is deliberate: it catches the compounds
/// we cannot enumerate (`smtpPassword`, `webhookSecret`, `oauthRefreshToken`)
/// without a list that goes stale every time the API grows a field.
///
/// Over-redaction is the intended failure mode here. A preview that hides a
/// value the caller already knows costs them nothing; a preview that prints a
/// credential into a CI log or a support ticket cannot be taken back.
const SENSITIVE_BODY_KEY_PARTS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "authkey",
    "privatekey",
    "integrationkey",
    "routingkey",
    "credential",
    "authorization",
    "cookie",
    "signature",
];

pub const REDACTED: &str = "<redacted>";

/// Normalize a JSON key for matching: lowercase, minus separators.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn is_sensitive_body_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    SENSITIVE_BODY_KEY_PARTS
        .iter()
        .any(|part| normalized.contains(part))
}

/// Recursively replace the value of every sensitive key with [`REDACTED`].
///
/// Structure is preserved exactly — a redacted field keeps its key and its
/// position, so the caller can still see *that* they are sending a password,
/// just not which one. Arrays are walked too: a body carrying a list of
/// connectors redacts the secret inside each element.
///
/// The whole subtree under a sensitive key is replaced, not just scalars. A
/// field named `credentials` holding an object would otherwise leak every value
/// inside it.
pub fn redact_body(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let redacted = if is_sensitive_body_key(k) {
                        Value::String(REDACTED.to_string())
                    } else {
                        redact_body(v)
                    };
                    (k.clone(), redacted)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_body).collect()),
        other => other.clone(),
    }
}

/// Which command is being previewed. `command` is the user-facing path
/// (`alerts delete`), `id` the OpenAPI `operationId` where one exists.
#[derive(Debug, Clone)]
pub struct OperationRef {
    pub id: Option<String>,
    pub command: String,
    pub description: Option<String>,
}

impl OperationRef {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            id: None,
            command: command.into(),
            description: None,
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn with_description(mut self, description: Option<String>) -> Self {
        self.description = description;
        self
    }

    fn to_json(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "command": self.command,
            "description": self.description,
        })
    }
}

/// The request as it would be sent — before credentials are attached.
#[derive(Debug, Clone)]
pub struct RequestPreview {
    pub method: String,
    pub url: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
}

impl RequestPreview {
    fn to_json(&self) -> Value {
        serde_json::json!({
            "method": self.method,
            "url": self.url,
            "query": self.query.iter().map(|(k, v)| serde_json::json!({
                "name": k, "value": v,
            })).collect::<Vec<_>>(),
            "headers": self.headers.iter().map(|(k, v)| serde_json::json!({
                "name": k, "value": redact(k, v),
            })).collect::<Vec<_>>(),
            "body": self.body.as_ref().map(redact_body),
        })
    }
}

fn redact<'a>(name: &str, value: &'a str) -> &'a str {
    if SENSITIVE_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
        REDACTED
    } else {
        value
    }
}

/// Build the envelope. `status` is one of [`STATUS_DRY_RUN`] or
/// [`STATUS_CONFIRMATION_REQUIRED`].
pub fn envelope(
    status: &str,
    operation: &OperationRef,
    classification: Classification,
    request: &RequestPreview,
) -> Value {
    serde_json::json!({
        "status": status,
        "operation": operation.to_json(),
        "classification": classification.to_json(),
        "request": request.to_json(),
        "confirmation": {
            "required": classification.destructive,
            "flag": CONFIRM_FLAG,
        },
    })
}

/// A refusal always states that confirmation is required, even if the caller's
/// classification somehow said otherwise — we would not be here if it weren't.
pub fn refusal(
    operation: &OperationRef,
    classification: Classification,
    request: &RequestPreview,
) -> Value {
    let mut value = envelope(
        STATUS_CONFIRMATION_REQUIRED,
        operation,
        classification,
        request,
    );
    value["confirmation"]["required"] = Value::Bool(true);
    value
}

pub fn dry_run(
    operation: &OperationRef,
    classification: Classification,
    request: &RequestPreview,
) -> Value {
    envelope(STATUS_DRY_RUN, operation, classification, request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> RequestPreview {
        RequestPreview {
            method: "DELETE".into(),
            url: "https://api.ilert.com/api/alerts/12345".into(),
            query: vec![("include".into(), "logs".into())],
            headers: vec![
                ("Authorization".into(), "Bearer super-secret".into()),
                ("x-team-context".into(), "42".into()),
            ],
            body: None,
        }
    }

    fn sample_op() -> OperationRef {
        OperationRef::new("alerts delete")
            .with_id("deleteAlert")
            .with_description(Some("Delete alert 12345".into()))
    }

    #[test]
    fn dry_run_and_refusal_share_one_schema() {
        let c = Classification::new(false, true, true);
        let a = dry_run(&sample_op(), c, &sample_request());
        let b = refusal(&sample_op(), c, &sample_request());

        let keys = |v: &Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys(&a), keys(&b));
        assert_eq!(keys(&a["request"]), keys(&b["request"]));
        assert_eq!(a["status"], STATUS_DRY_RUN);
        assert_eq!(b["status"], STATUS_CONFIRMATION_REQUIRED);
    }

    #[test]
    fn credentials_never_reach_the_envelope() {
        let value = dry_run(
            &sample_op(),
            Classification::new(false, true, true),
            &sample_request(),
        );
        let rendered = serde_json::to_string(&value).unwrap();
        assert!(!rendered.contains("super-secret"));
        assert!(rendered.contains(REDACTED));
        // Non-sensitive headers survive intact.
        assert!(rendered.contains("x-team-context"));
        assert!(rendered.contains("42"));
    }

    #[test]
    fn redaction_is_case_insensitive() {
        assert_eq!(redact("AUTHORIZATION", "x"), REDACTED);
        assert_eq!(redact("Cookie", "x"), REDACTED);
        assert_eq!(redact("Accept", "x"), "x");
    }

    #[test]
    fn body_redaction_matches_keys_in_any_casing_or_separator_style() {
        for key in [
            "password",
            "Password",
            "apiKey",
            "api_key",
            "API-KEY",
            "clientSecret",
            "refresh_token",
            "integrationKey",
            "smtpPassword",
            "webhookSecret",
        ] {
            let body = serde_json::json!({ key: "leaked" });
            let rendered = serde_json::to_string(&redact_body(&body)).unwrap();
            assert!(!rendered.contains("leaked"), "{key} was not redacted");
            assert!(rendered.contains(REDACTED), "{key} lost its placeholder");
        }
    }

    #[test]
    fn body_redaction_leaves_ordinary_fields_alone() {
        let body = serde_json::json!({
            "summary": "Disk full",
            "priority": "HIGH",
            "count": 3,
            "enabled": true,
        });
        assert_eq!(redact_body(&body), body);
    }

    #[test]
    fn body_redaction_reaches_nested_objects_and_arrays() {
        let body = serde_json::json!({
            "name": "connector",
            "params": { "url": "https://example.com/gateway", "authToken": "leaked-a" },
            "targets": [
                { "id": 1, "password": "leaked-b" },
                { "id": 2, "password": "leaked-c" },
            ],
        });

        let rendered = serde_json::to_string(&redact_body(&body)).unwrap();
        for leaked in ["leaked-a", "leaked-b", "leaked-c"] {
            assert!(!rendered.contains(leaked), "{leaked} survived redaction");
        }
        // Structure and non-sensitive values are preserved.
        assert!(rendered.contains("connector"));
        assert!(rendered.contains("https://example.com/gateway"));
        assert!(rendered.contains("targets"));
    }

    /// A sensitive key holding an object must lose the whole subtree — redacting
    /// only scalars would print every value inside `credentials`.
    #[test]
    fn a_sensitive_key_redacts_its_entire_subtree() {
        let body = serde_json::json!({
            "credentials": { "user": "admin", "pass": "leaked", "nested": [1, 2, 3] },
        });
        let redacted = redact_body(&body);
        assert_eq!(redacted["credentials"], Value::String(REDACTED.into()));
        let rendered = serde_json::to_string(&redacted).unwrap();
        assert!(!rendered.contains("leaked"));
        assert!(!rendered.contains("admin"));
    }

    #[test]
    fn the_envelope_body_is_redacted() {
        let request = RequestPreview {
            method: "POST".into(),
            url: "https://api.ilert.com/api/connectors".into(),
            query: vec![],
            headers: vec![],
            body: Some(serde_json::json!({
                "name": "smtp",
                "params": { "password": "hunter2" },
            })),
        };
        let rendered = serde_json::to_string(&dry_run(
            &sample_op(),
            Classification::new(false, false, false),
            &request,
        ))
        .unwrap();

        assert!(!rendered.contains("hunter2"));
        assert!(rendered.contains(REDACTED));
        // The field itself still shows, so the preview stays a faithful shape.
        assert!(rendered.contains("password"));
        assert!(rendered.contains("smtp"));
    }

    #[test]
    fn confirmation_tracks_destructiveness() {
        let read_only = Classification::new(true, false, true);
        let value = dry_run(&sample_op(), read_only, &sample_request());
        assert_eq!(value["confirmation"]["required"], Value::Bool(false));
        assert_eq!(value["confirmation"]["flag"], CONFIRM_FLAG);

        let value = refusal(&sample_op(), read_only, &sample_request());
        assert_eq!(value["confirmation"]["required"], Value::Bool(true));
    }

    #[test]
    fn no_shell_command_is_reconstructed() {
        let rendered = serde_json::to_string(&refusal(
            &sample_op(),
            Classification::new(false, true, true),
            &sample_request(),
        ))
        .unwrap();
        assert!(!rendered.contains("confirmCommand"));
        assert!(!rendered.contains("ilert alerts delete"));
    }
}
