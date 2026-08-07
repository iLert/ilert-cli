use std::time::Duration;

use anyhow::Result;
use reqwest::{Client, Method, Response};
use serde_json::Value;

use crate::errors::CliError;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// HTTP status codes that are safe to retry.
fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

/// Headers a caller may never set, wherever the value came from: `-H`, an
/// OpenAPI header parameter, or `ops run --param`.
///
/// The first two would let a header override authentication; `host` re-targets
/// the request; `content-length` is computed for us and a wrong value truncates
/// or hangs the body; `x-team-context` decides which team's data the request
/// touches and belongs to `--team-context`, which is resolved and validated.
pub const RESERVED_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "host",
    "content-length",
    "x-team-context",
];

pub fn is_reserved_header(name: &str) -> bool {
    RESERVED_HEADERS.contains(&name.trim().to_ascii_lowercase().as_str())
}

/// Reject a reserved header name.
///
/// Called both where headers are parsed (so the error names the flag the user
/// typed) and again in [`HttpClient::send_once`], which is the last point one
/// can reach the wire — a new code path that forgets the first check still
/// cannot smuggle a header past this one.
pub fn ensure_not_reserved(name: &str) -> Result<()> {
    if is_reserved_header(name) {
        return Err(CliError::user(format!(
            "Header '{name}' is reserved and cannot be overridden. \
             Use --api-key/--team-context or a profile instead."
        ))
        .into());
    }
    Ok(())
}

/// A decoded response body that remembers whether it was JSON to begin with.
///
/// The distinction cannot be recovered from the `Value` afterwards: a body of
/// `"ok"` is a valid JSON string and a body of `ok` is plain text, and both
/// decode to the same `Value::String`. Only the first is something `--jq` can
/// meaningfully filter, so the decoder records which one it saw.
#[derive(Clone, Debug)]
pub struct ResponseBody {
    value: Value,
    is_json: bool,
}

impl ResponseBody {
    /// A body that parsed as JSON — including a bare JSON string or `null`.
    pub fn json(value: Value) -> Self {
        Self {
            value,
            is_json: true,
        }
    }

    /// A body that did not parse as JSON, carried along as a string so it can
    /// still be shown to the caller.
    pub fn text(text: String) -> Self {
        Self {
            value: Value::String(text),
            is_json: false,
        }
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn into_value(self) -> Value {
        self.value
    }

    pub fn is_json(&self) -> bool {
        self.is_json
    }
}

/// A full response, for callers that need more than the decoded body — `ilert
/// api --include` prints the status line and headers, and does so through this
/// client rather than bypassing shared auth and retry behavior.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ResponseBody,
}

pub struct HttpClient {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    team_context: Option<String>,
    extra_headers: Vec<(String, String)>,
    debug: bool,
    verbose: bool,
}

impl HttpClient {
    pub fn new(base_url: String, api_key: Option<String>, team_context: Option<String>) -> Self {
        let client = crate::client::builder()
            .timeout(REQUEST_TIMEOUT)
            // A REST API has no business redirecting us, and following one
            // would forward the Authorization header to wherever it points.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("Failed to build HTTP client");

        Self {
            client,
            base_url,
            api_key,
            team_context,
            extra_headers: Vec::new(),
            debug: false,
            verbose: false,
        }
    }

    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// Print request/response headers to stderr, with credentials redacted.
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Headers applied to every request from this client (`--header`, global).
    /// Per-operation headers are applied afterwards and therefore win, except
    /// for the reserved ones rejected at parse time.
    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        headers: &[(String, String)],
        body: Option<Value>,
    ) -> Result<(u16, ResponseBody)> {
        let response = self
            .request_full(method, path, query, headers, body)
            .await?;
        Ok((response.status, response.body))
    }

    /// Send, and turn an error status into an [`CliError::Http`].
    pub async fn request_full(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        headers: &[(String, String)],
        body: Option<Value>,
    ) -> Result<HttpResponse> {
        let response = self.request_raw(method, path, query, headers, body).await?;
        if response.status >= 400 {
            return Err(http_error(&response));
        }
        Ok(response)
    }

    /// Send and return the response as it arrived, error statuses included.
    ///
    /// `ilert api --include` needs this: the status line and headers of a 4xx
    /// are exactly what a caller debugging one is asking for, and collapsing the
    /// response into an error message throws them away. Retries still apply, so
    /// a 503 is only surfaced once the retry budget is spent.
    pub async fn request_raw(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        headers: &[(String, String)],
        body: Option<Value>,
    ) -> Result<HttpResponse> {
        let url = format!("{}{}", self.base_url, path);
        let mut last: Option<HttpResponse> = None;
        let mut last_err: Option<anyhow::Error> = None;

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let backoff = INITIAL_BACKOFF * 2u32.pow(attempt - 1);
                tokio::time::sleep(backoff).await;
            }

            let result = self
                .send_once(&method, &url, query, headers, body.clone())
                .await;

            match result {
                Ok(response) => {
                    if is_retryable_status(response.status) && attempt < MAX_RETRIES {
                        last = Some(response);
                        continue;
                    }
                    return Ok(response);
                }
                Err(e) => {
                    // Retry on network/timeout errors
                    if attempt < MAX_RETRIES && is_retryable_error(&e) {
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        // Retries exhausted: report the last response we actually got, so a
        // caller still sees the real status rather than a generic failure.
        if let Some(response) = last {
            return Ok(response);
        }
        Err(last_err.unwrap_or_else(|| CliError::user("Request failed after retries").into()))
    }

    async fn send_once(
        &self,
        method: &Method,
        url: &str,
        query: &[(String, String)],
        headers: &[(String, String)],
        body: Option<Value>,
    ) -> Result<HttpResponse> {
        let mut req = self.client.request(method.clone(), url);

        if let Some(key) = &self.api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        if let Some(ctx) = &self.team_context {
            req = req.header("x-team-context", ctx);
        }

        req = req.header("Accept", "application/json");

        for (k, v) in query {
            req = req.query(&[(k, v)]);
        }

        // Global headers first so per-operation headers take precedence.
        for (k, v) in self.extra_headers.iter().chain(headers.iter()) {
            ensure_not_reserved(k)?;
            req = req.header(k.as_str(), v.as_str());
        }

        if let Some(ref body) = body {
            if self.debug {
                eprintln!(
                    "debug: {} {} body={}",
                    method,
                    url,
                    serde_json::to_string(&crate::preview::redact_body(body)).unwrap_or_default()
                );
            }
            // The unredacted body goes on the wire — redaction is a property of
            // what we *print*, never of what we send.
            req = req.json(body);
        } else if self.debug {
            eprintln!("debug: {} {}", method, url);
        }

        if self.verbose {
            eprintln!("> {method} {url}");
            for (k, v) in self.effective_request_headers(headers, body.is_some()) {
                eprintln!("> {k}: {}", redact_header(&k, &v));
            }
        }

        let response: Response = req.send().await?;
        let status = response.status().as_u16();
        let response_headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    v.to_str().unwrap_or("<binary>").to_string(),
                )
            })
            .collect();

        if self.verbose {
            eprintln!("< HTTP {status}");
            for (k, v) in &response_headers {
                eprintln!("< {k}: {}", redact_header(k, v));
            }
        }

        let text = response.text().await?;

        if self.debug {
            eprintln!(
                "debug: <- {} ({} bytes) {}",
                status,
                text.len(),
                debug_body_preview(&text)
            );
        }

        let body = if text.is_empty() {
            ResponseBody::json(Value::Null)
        } else {
            match serde_json::from_str(&text) {
                Ok(value) => ResponseBody::json(value),
                Err(_) => ResponseBody::text(text),
            }
        };

        Ok(HttpResponse {
            status,
            headers: response_headers,
            body,
        })
    }

    /// Every header this client will actually put on the request, in the order
    /// it applies them.
    ///
    /// `--verbose` exists to answer "what did you send?", so it has to include
    /// the ones the client adds on the caller's behalf — the identifying
    /// headers from the shared builder, `Accept`, and the credential and team
    /// context resolved from the profile. Values are redacted by
    /// [`redact_header`] at the point of printing, not here, so this stays a
    /// faithful description of the request.
    fn effective_request_headers(
        &self,
        per_request: &[(String, String)],
        has_body: bool,
    ) -> Vec<(String, String)> {
        let mut out = vec![
            ("User-Agent".to_string(), crate::client::user_agent()),
            ("Accept".to_string(), "application/json".to_string()),
        ];
        if has_body {
            out.push(("Content-Type".to_string(), "application/json".to_string()));
        }
        if self.api_key.is_some() {
            out.push(("Authorization".to_string(), "Bearer ...".to_string()));
        }
        if let Some(ctx) = &self.team_context {
            out.push(("x-team-context".to_string(), ctx.clone()));
        }
        out.extend(self.extra_headers.iter().cloned());
        out.extend(per_request.iter().cloned());
        out
    }
}

/// Turn an error-status response into the error the rest of the CLI reports.
pub fn http_error(response: &HttpResponse) -> anyhow::Error {
    CliError::Http {
        status: response.status,
        message: extract_error_message(response.body.value(), response.status),
        details: Some(response.body.value().clone()),
    }
    .into()
}

/// Values `--verbose` must never print.
///
/// Wider than the preview module's redaction list by one entry:
/// `x-team-context` is an identifier rather than a credential, so a preview
/// shows it (you need to know which team a request would touch), but verbose
/// output is routinely pasted into issues and chat, where it is not worth
/// leaking.
fn redact_header(name: &str, value: &str) -> String {
    const SENSITIVE: &[&str] = &[
        "authorization",
        "proxy-authorization",
        "cookie",
        "set-cookie",
        "x-api-key",
        "api-key",
        "x-auth-token",
        "x-team-context",
    ];
    if SENSITIVE.contains(&name.to_ascii_lowercase().as_str()) {
        crate::preview::REDACTED.to_string()
    } else {
        value.to_string()
    }
}

/// How much of a response body `--debug` shows.
const DEBUG_BODY_PREVIEW_CHARS: usize = 200;

/// The response body as `--debug` prints it: sensitive keys redacted, then
/// truncated.
///
/// Redaction has to happen before truncation — cutting a JSON document at 200
/// characters and *then* looking for keys would miss every credential past the
/// boundary while still printing it.
///
/// A body that is not JSON cannot be key-redacted, so it is only truncated.
/// That is the honest limit of this approach: `--debug` on an endpoint that
/// returns a bare token as plain text will still show it.
fn debug_body_preview(text: &str) -> String {
    let rendered = match serde_json::from_str::<Value>(text) {
        Ok(value) => serde_json::to_string(&crate::preview::redact_body(&value))
            .unwrap_or_else(|_| text.to_string()),
        Err(_) => text.to_string(),
    };
    // Truncate on a char boundary (byte slicing can panic on UTF-8).
    rendered.chars().take(DEBUG_BODY_PREVIEW_CHARS).collect()
}

fn extract_error_message(value: &Value, status: u16) -> String {
    if let Some(msg) = value.get("message").and_then(|v| v.as_str()) {
        return msg.to_string();
    }
    if let Some(msg) = value.get("error").and_then(|v| v.as_str()) {
        return msg.to_string();
    }
    format!("Request failed with status {status}")
}

fn is_retryable_error(err: &anyhow::Error) -> bool {
    if let Some(reqwest_err) = err.downcast_ref::<reqwest::Error>() {
        return reqwest_err.is_timeout() || reqwest_err.is_connect() || reqwest_err.is_request();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_response_headers_are_redacted_in_verbose_output() {
        for name in [
            "Set-Cookie",
            "Authorization",
            "Cookie",
            "x-team-context",
            "X-Team-Context",
        ] {
            assert_eq!(
                redact_header(name, "secret"),
                crate::preview::REDACTED,
                "{name} must not be printed"
            );
        }
        assert_eq!(
            redact_header("Content-Type", "application/json"),
            "application/json"
        );
    }

    #[test]
    fn verbose_lists_the_headers_the_client_adds_itself() {
        let client = HttpClient::new(
            "https://api.ilert.com".into(),
            Some("secret-key".into()),
            Some("42".into()),
        )
        .with_extra_headers(vec![("X-Request-Id".into(), "abc".into())]);

        let names: Vec<String> = client
            .effective_request_headers(&[("If-Match".into(), "v1".into())], true)
            .into_iter()
            .map(|(k, _)| k.to_ascii_lowercase())
            .collect();

        for expected in [
            "user-agent",
            "accept",
            "content-type",
            "authorization",
            "x-team-context",
            "x-request-id",
            "if-match",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
    }

    #[test]
    fn verbose_never_prints_a_credential_or_the_team_context() {
        let client = HttpClient::new(
            "https://api.ilert.com".into(),
            Some("super-secret".into()),
            Some("team-42".into()),
        );
        let rendered: String = client
            .effective_request_headers(&[], false)
            .into_iter()
            .map(|(k, v)| format!("{k}: {}\n", redact_header(&k, &v)))
            .collect();

        assert!(!rendered.contains("super-secret"));
        assert!(!rendered.contains("team-42"));
        assert!(rendered.contains(crate::preview::REDACTED));
    }

    #[test]
    fn debug_redacts_sensitive_keys_in_a_json_response_body() {
        let body = r#"{"id":7,"name":"smtp","params":{"password":"hunter2"}}"#;
        let rendered = debug_body_preview(body);
        assert!(!rendered.contains("hunter2"));
        assert!(rendered.contains(crate::preview::REDACTED));
        assert!(rendered.contains("smtp"));
    }

    /// Redaction runs before truncation, so a credential sitting past the
    /// preview boundary is removed rather than merely cut off.
    #[test]
    fn debug_redacts_before_truncating() {
        let padding = "x".repeat(400);
        let body = format!(r#"{{"summary":"{padding}","apiKey":"leaked"}}"#);
        let rendered = debug_body_preview(&body);
        assert!(!rendered.contains("leaked"));
        assert_eq!(rendered.chars().count(), DEBUG_BODY_PREVIEW_CHARS);
    }

    #[test]
    fn debug_truncates_a_non_json_body_without_panicking_on_utf8() {
        let body = "ü".repeat(400);
        let rendered = debug_body_preview(&body);
        assert_eq!(rendered.chars().count(), DEBUG_BODY_PREVIEW_CHARS);
    }

    #[test]
    fn reserved_headers_are_rejected_at_the_wire() {
        for name in ["Authorization", "authorization", "HOST", "x-team-context"] {
            assert!(ensure_not_reserved(name).is_err(), "{name} must be refused");
        }
        assert!(ensure_not_reserved("X-Request-Id").is_ok());
    }
}
