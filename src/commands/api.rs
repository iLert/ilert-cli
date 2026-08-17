//! `ilert api <path>` — a `gh api`-style escape hatch.
//!
//! The spec-bound commands cannot reach an endpoint that the cached spec does
//! not know about, which is exactly the situation you are in when the API has
//! moved ahead of the spec. This takes a **path**, not an operation ID, and is a
//! thin wrapper over the shared `HttpClient` — same auth, same retry, same
//! profile resolution, same mode detection and dry-run handling. It is not a
//! second HTTP client.
//!
//! Operation-ID execution now lives at `ilert ops run <operation-id>`.

use anyhow::Result;
use clap::{Arg, ArgAction, ArgMatches, Command};
use reqwest::Method;
use serde_json::Value;
use url::Url;

use crate::errors::CliError;

/// Methods that carry a request body. Fields and `--input` become a JSON body
/// for these and query parameters for everything else.
const BODY_METHODS: &[&str] = &["POST", "PUT", "PATCH"];

pub fn command() -> Command {
    Command::new("api")
        .about("Send a request to an arbitrary API path")
        .arg_required_else_help(true)
        .after_help(
            "Examples:\n  \
             ilert api /api/alerts\n  \
             ilert api /api/alerts -X GET --jq '.[].summary'\n  \
             ilert api /api/alerts -X POST -F summary=Test -F 'priority:=\"HIGH\"'\n  \
             echo '{\"summary\":\"New\"}' | ilert api /api/alerts -X POST --input -\n\n\
             Paths must start with '/'. To run a spec operation by ID, use 'ilert ops run <id>'.",
        )
        .arg(
            Arg::new("target")
                .required(true)
                .help("API path, starting with '/' (e.g. /api/alerts)"),
        )
        .arg(
            Arg::new("method")
                .short('X')
                .long("method")
                .value_name("METHOD")
                .help("HTTP method (default: GET, or POST when fields/input are present)"),
        )
        .arg(
            Arg::new("field")
                .short('F')
                .long("field")
                .value_name("KEY=VALUE")
                .action(ArgAction::Append)
                .help("Add a field: key=value is a string, key:=value is parsed as JSON"),
        )
        .arg(
            Arg::new("input")
                .long("input")
                .value_name("FILE")
                .help("Read the request body from a file, or '-' for stdin"),
        )
        .arg(
            Arg::new("include")
                .short('i')
                .long("include")
                .action(ArgAction::SetTrue)
                .help("Print the status and response headers alongside the body"),
        )
        .arg(
            Arg::new("verbose")
                .long("verbose")
                .action(ArgAction::SetTrue)
                .help("Print request and response headers to stderr"),
        )
        .arg(
            Arg::new("dry-run")
                .long("dry-run")
                .action(ArgAction::SetTrue)
                .help("Preview the request without sending"),
        )
        // Deprecated: kept only so `ilert api <operation-id> --set k=v` keeps
        // working for one release. Use `ilert ops run` instead.
        .arg(Arg::new("body").long("body").hide(true))
        .arg(Arg::new("body-file").long("body-file").hide(true))
        .arg(
            Arg::new("set")
                .long("set")
                .action(ArgAction::Append)
                .hide(true),
        )
        .arg(
            Arg::new("param")
                .long("param")
                .action(ArgAction::Append)
                .hide(true),
        )
}

/// A resolved passthrough request. `path` is relative to the configured base
/// URL, so it goes through the shared client unchanged.
#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub method: Method,
    pub path: String,
    pub url: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
}

pub fn build_request(matches: &ArgMatches, base_url: &str) -> Result<ApiRequest> {
    let target = matches.get_one::<String>("target").expect("required");

    let fields: Vec<(String, Value)> = matches
        .get_many::<String>("field")
        .map(|values| values.map(|v| parse_field(v)).collect::<Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();

    let input = matches.get_one::<String>("input");

    if input.is_some() && !fields.is_empty() {
        return Err(CliError::user(
            "--input and -F/--field are mutually exclusive: pick one way to supply the body.",
        )
        .into());
    }

    let method = resolve_method(
        matches.get_one::<String>("method").map(String::as_str),
        !fields.is_empty() || input.is_some(),
    )?;

    let carries_body = BODY_METHODS.contains(&method.as_str());

    let (path, mut query) = resolve_target(base_url, target)?;

    let mut body: Option<Value> = None;

    if let Some(source) = input {
        if !carries_body {
            return Err(CliError::user(format!(
                "--input needs a method that carries a body ({}), not {method}.",
                BODY_METHODS.join(", ")
            ))
            .into());
        }
        body = Some(read_input(source)?);
    } else if !fields.is_empty() {
        if carries_body {
            let mut object = serde_json::Map::new();
            for (key, value) in fields {
                object.insert(key, value);
            }
            body = Some(Value::Object(object));
        } else {
            for (key, value) in fields {
                query.push((key, field_as_query_value(&value)));
            }
        }
    }

    // `-H/--header` is a global flag: those headers are attached by the shared
    // client, so they are deliberately not repeated per request here.
    let headers = Vec::new();

    let url = format!("{}{}", base_url.trim_end_matches('/'), path);

    Ok(ApiRequest {
        method: method
            .parse()
            .map_err(|_| CliError::user(format!("Invalid HTTP method: {method}")))?,
        path,
        url,
        query,
        headers,
        body,
    })
}

fn resolve_method(explicit: Option<&str>, has_payload: bool) -> Result<String> {
    let method = match explicit {
        Some(m) => m.trim().to_ascii_uppercase(),
        None if has_payload => "POST".to_string(),
        None => "GET".to_string(),
    };
    if method.is_empty() || !method.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(CliError::user(format!("Invalid HTTP method: {method}")).into());
    }
    Ok(method)
}

/// Resolve a user-supplied path against the configured base URL.
///
/// The point of this function is that no input may move the request to another
/// origin — that is how a credential ends up somewhere it was never meant to go.
pub fn resolve_target(base_url: &str, raw: &str) -> Result<(String, Vec<(String, String)>)> {
    if raw.starts_with("//") {
        return Err(CliError::user(format!(
            "Refusing scheme-relative path '{raw}'. Paths must start with a single '/'."
        ))
        .into());
    }
    if !raw.starts_with('/') {
        return Err(CliError::user(format!(
            "API path must start with '/': got '{raw}'. To run a spec operation by ID, use 'ilert ops run {raw}'."
        ))
        .into());
    }
    // A leading '/' means this cannot parse as an absolute URL, but check
    // explicitly so the error names the real problem.
    if Url::parse(raw).is_ok() {
        return Err(CliError::user(format!(
            "Refusing absolute URL '{raw}'. Pass a path and let --base-url decide the host."
        ))
        .into());
    }

    let base = Url::parse(base_url)
        .map_err(|_| CliError::user(format!("Invalid base URL: {base_url}")))?;

    let candidate = format!("{}{}", base_url.trim_end_matches('/'), raw);
    let full = Url::parse(&candidate).map_err(|_| {
        CliError::user(format!("Could not resolve path '{raw}' against {base_url}"))
    })?;

    if full.origin() != base.origin() {
        return Err(CliError::user(format!(
            "Refusing to send a request to a different origin than {base_url}."
        ))
        .into());
    }

    // Url normalises `..` segments during parsing, so this is the path the
    // server would actually see.
    let base_path = base.path().trim_end_matches('/');
    let mut path = full.path().to_string();
    if !base_path.is_empty()
        && let Some(stripped) = path.strip_prefix(base_path)
    {
        path = stripped.to_string();
    }
    if !path.starts_with('/') {
        path.insert(0, '/');
    }

    let query: Vec<(String, String)> = full
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    Ok((path, query))
}

/// `key=value` is a string; `key:=value` is parsed as JSON.
fn parse_field(raw: &str) -> Result<(String, Value)> {
    let equals = raw.find('=');
    let json_marker = raw.find(":=");

    // ':=' only counts when its '=' is the first '=' in the string — otherwise
    // the ':=' lives inside a value, as in `note=a:=b`.
    if let (Some(eq), Some(marker)) = (equals, json_marker)
        && marker + 1 == eq
    {
        let key = &raw[..marker];
        let value = &raw[marker + 2..];
        let parsed: Value = serde_json::from_str(value).map_err(|e| {
            CliError::user(format!(
                "Invalid JSON in field '{key}': {e}. Use key=value for a plain string."
            ))
        })?;
        return Ok((validate_key(key)?, parsed));
    }

    let (key, value) = raw.split_once('=').ok_or_else(|| {
        CliError::user(format!(
            "Invalid field '{raw}'. Expected key=value (string) or key:=value (JSON)."
        ))
    })?;
    Ok((validate_key(key)?, Value::String(value.to_string())))
}

fn validate_key(key: &str) -> Result<String> {
    if key.trim().is_empty() {
        return Err(CliError::user("Field names cannot be empty.").into());
    }
    Ok(key.to_string())
}

fn field_as_query_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn parse_header(raw: &str) -> Result<(String, String)> {
    if raw.contains('\n') || raw.contains('\r') {
        return Err(CliError::user("Header values cannot contain newlines.").into());
    }
    let (name, value) = raw.split_once(':').ok_or_else(|| {
        CliError::user(format!("Invalid header '{raw}'. Expected \"Key: Value\"."))
    })?;
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() {
        return Err(CliError::user(format!("Invalid header '{raw}': empty name.")).into());
    }
    crate::http::ensure_not_reserved(name)?;
    Ok((name.to_string(), value.to_string()))
}

fn read_input(source: &str) -> Result<Value> {
    let raw = if source == "-" {
        std::io::read_to_string(std::io::stdin())
            .map_err(|e| CliError::user(format!("Failed to read body from stdin: {e}")))?
    } else {
        std::fs::read_to_string(source)
            .map_err(|e| CliError::user(format!("Failed to read {source}: {e}")))?
    };
    serde_json::from_str(&raw).map_err(|e| CliError::user(format!("Invalid JSON body: {e}")).into())
}

/// Global `-H/--header` values, validated the same way as the per-command ones.
pub fn parse_global_headers(matches: &ArgMatches) -> Result<Vec<(String, String)>> {
    matches
        .get_many::<String>("header")
        .map(|values| values.map(|v| parse_header(v)).collect::<Result<Vec<_>>>())
        .transpose()
        .map(Option::unwrap_or_default)
}

/// `--include`: the status line and response headers, ahead of the body.
///
/// Header values are server-controlled and printed one per line, so an escape
/// or a `\r` in one could forge a header that was never sent — or, with OSC 52,
/// leave something in the user's clipboard.
pub fn print_response_meta(status: u16, headers: &[(String, String)]) {
    println!("HTTP {status}");
    for (name, value) in headers {
        println!(
            "{}: {}",
            crate::sanitize::terminal_text(name),
            crate::sanitize::terminal_text(value)
        );
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BASE: &str = "https://api.ilert.com";

    #[test]
    fn plain_paths_resolve() {
        let (path, query) = resolve_target(BASE, "/api/alerts").unwrap();
        assert_eq!(path, "/api/alerts");
        assert!(query.is_empty());
    }

    #[test]
    fn query_in_the_path_is_extracted() {
        let (path, query) = resolve_target(BASE, "/api/alerts?states=PENDING&limit=5").unwrap();
        assert_eq!(path, "/api/alerts");
        assert_eq!(
            query,
            vec![
                ("states".to_string(), "PENDING".to_string()),
                ("limit".to_string(), "5".to_string()),
            ]
        );
    }

    #[test]
    fn absolute_urls_are_refused() {
        let err = resolve_target(BASE, "https://evil.example/api").unwrap_err();
        assert!(err.to_string().contains("must start with '/'"));
    }

    #[test]
    fn scheme_relative_paths_are_refused() {
        let err = resolve_target(BASE, "//evil.example/api").unwrap_err();
        assert!(err.to_string().contains("scheme-relative"));
    }

    #[test]
    fn traversal_cannot_escape_the_origin() {
        // Url normalises this; whatever it lands on must still be our origin.
        let (path, _) = resolve_target(BASE, "/api/../../../etc/passwd").unwrap();
        assert!(path.starts_with('/'));
        assert!(!path.contains(".."));
    }

    #[test]
    fn a_base_url_path_prefix_is_not_duplicated() {
        let (path, _) = resolve_target("https://example.com/gateway", "/api/alerts").unwrap();
        assert_eq!(path, "/api/alerts");
    }

    #[test]
    fn operation_ids_get_a_pointer_to_ops_run() {
        let err = resolve_target(BASE, "getAlert").unwrap_err();
        assert!(err.to_string().contains("ops run getAlert"));
    }

    #[test]
    fn string_and_json_fields_are_distinguished() {
        assert_eq!(
            parse_field("summary=Test").unwrap(),
            ("summary".to_string(), json!("Test"))
        );
        assert_eq!(
            parse_field("count:=5").unwrap(),
            ("count".to_string(), json!(5))
        );
        assert_eq!(
            parse_field("tags:=[\"a\",\"b\"]").unwrap(),
            ("tags".to_string(), json!(["a", "b"]))
        );
    }

    #[test]
    fn a_colon_equals_inside_a_value_stays_a_string() {
        assert_eq!(
            parse_field("note=a:=b").unwrap(),
            ("note".to_string(), json!("a:=b"))
        );
    }

    #[test]
    fn malformed_fields_are_errors() {
        assert!(parse_field("nope").is_err());
        assert!(parse_field("=value").is_err());
        assert!(parse_field("bad:=not json").is_err());
    }

    #[test]
    fn reserved_headers_are_refused() {
        for raw in [
            "Authorization: Bearer x",
            "authorization: x",
            "Host: evil.example",
            "x-team-context: 9",
        ] {
            assert!(parse_header(raw).is_err(), "{raw} should be refused");
        }
    }

    #[test]
    fn headers_cannot_smuggle_newlines() {
        assert!(parse_header("X-Test: a\r\nX-Injected: b").is_err());
    }

    #[test]
    fn ordinary_headers_parse() {
        assert_eq!(
            parse_header("X-Request-Id:  abc123 ").unwrap(),
            ("X-Request-Id".to_string(), "abc123".to_string())
        );
    }

    fn build(args: &[&str]) -> Result<ApiRequest> {
        let matches = command()
            .try_get_matches_from(std::iter::once("api").chain(args.iter().copied()))
            .expect("args parse");
        build_request(&matches, BASE)
    }

    #[test]
    fn fields_become_query_params_for_read_methods() {
        let req = build(&["/api/alerts", "-X", "GET", "-F", "states=PENDING"]).unwrap();
        assert_eq!(req.method, Method::GET);
        assert_eq!(
            req.query,
            vec![("states".to_string(), "PENDING".to_string())]
        );
        assert!(req.body.is_none());
    }

    #[test]
    fn fields_become_a_body_for_write_methods() {
        let req = build(&[
            "/api/alerts",
            "-X",
            "POST",
            "-F",
            "summary=Test",
            "-F",
            "count:=2",
        ])
        .unwrap();
        assert_eq!(req.method, Method::POST);
        assert!(req.query.is_empty());
        assert_eq!(req.body, Some(json!({"summary": "Test", "count": 2})));
    }

    #[test]
    fn a_payload_implies_post() {
        let req = build(&["/api/alerts", "-F", "summary=Test"]).unwrap();
        assert_eq!(req.method, Method::POST);
        assert_eq!(req.body, Some(json!({"summary": "Test"})));
    }

    #[test]
    fn a_body_needs_a_method_that_carries_one() {
        let err = build(&["/api/alerts", "-X", "GET", "--input", "body.json"]).unwrap_err();
        assert!(err.to_string().contains("carries a body"));
    }

    #[test]
    fn input_and_fields_cannot_be_combined() {
        let err = build(&[
            "/api/alerts",
            "-X",
            "POST",
            "-F",
            "a=1",
            "--input",
            "body.json",
        ])
        .unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn method_defaults_to_get_and_to_post_with_a_payload() {
        assert_eq!(resolve_method(None, false).unwrap(), "GET");
        assert_eq!(resolve_method(None, true).unwrap(), "POST");
        assert_eq!(resolve_method(Some("delete"), false).unwrap(), "DELETE");
        assert!(resolve_method(Some("GET /x"), false).is_err());
    }
}
