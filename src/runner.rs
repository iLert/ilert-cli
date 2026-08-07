use anyhow::Result;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Method;
use serde_json::Value;

use crate::errors::CliError;
use crate::http::{HttpClient, ResponseBody};
use crate::openapi::{Operation, ParamLocation, Parameter};
use crate::preview::{OperationRef, RequestPreview};

const DEFAULT_PAGE_SIZE: u64 = 50;
const MAX_PAGES: u64 = 200;

/// How this operation is named back to the caller in a preview or refusal.
pub fn operation_ref(operation: &Operation) -> OperationRef {
    OperationRef::new(format!("{} {}", operation.tag, operation.action))
        .with_id(operation.id.clone())
        .with_description(operation.summary.clone())
}

/// Turn a caller-supplied value into exactly one path segment.
///
/// Path parameters are substituted into a `{...}` placeholder in the operation's
/// path template, so a value carrying a separator does not fill the placeholder
/// — it re-targets the request. `--stdin` makes this reachable in bulk: the
/// preview shown before a batch describes the template, and every line then
/// picks its own path.
///
/// So: refuse the values that cannot be one segment (`/`, `\`, `.`, `..`,
/// control characters) with an error the caller can act on, and percent-encode
/// everything else. Encoding rather than refusing keeps legitimately odd
/// identifiers working — a username with a space or an `@` — while a `%2F`
/// smuggled in pre-encoded becomes a literal `%252F`, which the server reads as
/// an id that does not exist rather than as a separator.
pub fn path_segment(name: &str, value: &str) -> Result<String> {
    let refuse = |reason: &str| -> anyhow::Error {
        CliError::user(format!(
            "Invalid value for path parameter '{name}': '{value}' {reason}. \
             It has to identify a single resource."
        ))
        .into()
    };

    if value.is_empty() {
        return Err(refuse("is empty"));
    }
    if value == "." || value == ".." {
        return Err(refuse("is a relative path reference"));
    }
    if value.contains('/') || value.contains('\\') {
        return Err(refuse("contains a path separator"));
    }
    if value.chars().any(char::is_control) {
        return Err(refuse("contains a control character"));
    }

    Ok(encode_path_segment(value))
}

/// Percent-encode everything outside the RFC 3986 unreserved set.
fn encode_path_segment(value: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// How to turn command-line arguments into a request.
pub struct BuildOptions<'a> {
    pub base_url: &'a str,
    /// Interactive body prompts are only ever appropriate for a human at a
    /// terminal. In `ci`/`agent` mode, during a dry run, or when stdin is
    /// already carrying piped IDs, a missing required field has to surface as a
    /// usage error instead of a hanging prompt.
    pub allow_prompting: bool,
    /// `--stdin` supplies the `{id}` for each line it reads, so the path stays
    /// a template. The preview and the confirmation prompt then describe the
    /// shape of the requests rather than inventing an ID that was never given.
    pub templated_path: bool,
}

/// Resolve every request parameter from the command line.
///
/// This deliberately does not take an [`HttpClient`]: the request has to exist
/// before we decide whether we are allowed to send it, and building it must not
/// require a credential. `--dry-run` and a refused destructive command both stop
/// after this point, having touched neither the keyring nor the network.
pub fn build_params(
    operation: &Operation,
    args: &clap::ArgMatches,
    options: &BuildOptions<'_>,
) -> Result<RequestParams> {
    RequestParams::from_operation(operation, args, options)
}

pub struct OperationRunner<'a> {
    client: &'a HttpClient,
}

impl<'a> OperationRunner<'a> {
    pub fn new(client: &'a HttpClient) -> Self {
        Self { client }
    }

    /// Send an already-built request.
    ///
    /// Building and sending are separate steps because a confirmation refusal
    /// has to describe the exact request it is refusing. Re-building it would
    /// mean re-reading stdin or re-running an interactive prompt.
    pub async fn send(
        &self,
        operation: &Operation,
        req: &RequestParams,
    ) -> Result<(u16, ResponseBody)> {
        let method: Method = operation
            .method
            .parse()
            .map_err(|_| CliError::user(format!("Invalid HTTP method: {}", operation.method)))?;

        self.client
            .request(
                method,
                &req.path,
                &req.query,
                &req.headers,
                req.body.clone(),
            )
            .await
    }

    pub async fn execute_paginated(
        &self,
        operation: &Operation,
        req: &RequestParams,
        args: &clap::ArgMatches,
    ) -> Result<Value> {
        let method: Method = operation
            .method
            .parse()
            .map_err(|_| CliError::user(format!("Invalid HTTP method: {}", operation.method)))?;

        let page_size = args
            .get_one::<String>("max-results")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_PAGE_SIZE);

        let mut all_items: Vec<Value> = Vec::new();
        let mut start_index: u64 = args
            .get_one::<String>("start-index")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap(),
        );
        spinner.set_message("Fetching...");
        spinner.enable_steady_tick(std::time::Duration::from_millis(100));

        let mut page = 0u64;

        loop {
            let mut query = req.query.clone();
            upsert_query(&mut query, "start-index", &start_index.to_string());
            upsert_query(&mut query, "max-results", &page_size.to_string());

            let (_, body) = self
                .client
                .request(
                    method.clone(),
                    &req.path,
                    &query,
                    &req.headers,
                    req.body.clone(),
                )
                .await?;

            let items = extract_page_items(body.value());
            let count = items.len() as u64;
            all_items.extend(items);
            page += 1;

            spinner.set_message(format!("Fetched {} items...", all_items.len()));

            if count < page_size {
                break;
            }

            if page >= MAX_PAGES {
                spinner.finish_and_clear();
                eprintln!(
                    "{} Stopped after {} pages ({} items). Use --from/--until or other filters to narrow your query.",
                    "Warning:".yellow().bold(),
                    MAX_PAGES,
                    all_items.len()
                );
                break;
            }

            start_index += count;

            // Rate-limit: 200ms between page requests
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        spinner.finish_and_clear();

        Ok(Value::Array(all_items))
    }
}

/// All the resolved request parameters, extracted from clap args + operation definition.
pub struct RequestParams {
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
    method: String,
    base_url: String,
}

impl RequestParams {
    fn from_operation(
        operation: &Operation,
        args: &clap::ArgMatches,
        options: &BuildOptions<'_>,
    ) -> Result<Self> {
        let mut path = operation.path.clone();
        let mut query: Vec<(String, String)> = Vec::new();
        let mut headers: Vec<(String, String)> = Vec::new();

        // `ops run` and the deprecated `api <operation-id>` form dispatch an
        // operation whose parameters were never registered as clap args, so
        // every lookup has to tolerate an unknown id and fall back to --param.
        let overrides = param_overrides(args, operation)?;

        for param in &operation.parameters {
            let owned = args
                .try_get_one::<String>(&param.name)
                .ok()
                .flatten()
                .cloned()
                .or_else(|| {
                    overrides
                        .iter()
                        .find(|(k, _)| *k == param.name)
                        .map(|(_, v)| v.clone())
                });
            let value = owned.as_deref();

            match &param.location {
                ParamLocation::Path => match value {
                    Some(val) => {
                        let segment = path_segment(&param.name, val)?;
                        path = path.replace(&format!("{{{}}}", param.name), &segment);
                    }
                    // In `--stdin` mode every line supplies the path parameter,
                    // so the template is the honest description of the request.
                    None if options.templated_path => {}
                    None => {
                        return Err(CliError::user(format!(
                            "Missing required path parameter: {}",
                            param.name
                        ))
                        .into());
                    }
                },
                ParamLocation::Query => match value {
                    Some(val) => query.push((param.name.clone(), val.to_string())),
                    None if param.required => {
                        return Err(missing_parameter(operation, param, "query").into());
                    }
                    None => {}
                },
                ParamLocation::Header => match value {
                    Some(val) => {
                        // A spec-declared header parameter is still caller input,
                        // so it goes through the same gate as `-H`.
                        crate::http::ensure_not_reserved(&param.name)?;
                        headers.push((param.name.clone(), val.to_string()));
                    }
                    None if param.required => {
                        return Err(missing_parameter(operation, param, "header").into());
                    }
                    None => {}
                },
            }
        }

        let body = build_body(operation, args, options.allow_prompting)?;

        if operation.request_body_required && body.is_none() {
            return Err(CliError::user(format!(
                "{} {} requires a request body. Pass one with --body '<json>', --body-file <path>, \
                 or --set key=value.",
                operation.tag, operation.action
            ))
            .into());
        }

        Ok(Self {
            path,
            query,
            headers,
            body,
            method: operation.method.clone(),
            base_url: options.base_url.to_string(),
        })
    }

    /// The request as it would go out. Credentials are added by `HttpClient`
    /// and so are structurally absent here; `preview::envelope` redacts
    /// anything sensitive that still slips in.
    pub fn preview(&self) -> RequestPreview {
        RequestPreview {
            method: self.method.clone(),
            url: format!("{}{}", self.base_url, self.path),
            query: self.query.clone(),
            headers: self.headers.clone(),
            body: self.body.clone(),
        }
    }
}

fn missing_parameter(operation: &Operation, param: &Parameter, location: &str) -> CliError {
    let hint = param
        .description
        .as_deref()
        .map(|d| format!(" ({d})"))
        .unwrap_or_default();
    CliError::user(format!(
        "{} {} requires the {} parameter '{}'{}.",
        operation.tag, operation.action, location, param.name, hint
    ))
}

/// `--param name=value`, the escape hatch for operations dispatched by ID.
///
/// A typo here used to be silent: `--param alertId=7` on an operation whose
/// parameter is `id` simply did nothing and the request went out without it.
/// Anything we cannot place is now a usage error.
fn param_overrides(
    args: &clap::ArgMatches,
    operation: &Operation,
) -> Result<Vec<(String, String)>> {
    let Some(values) = args.try_get_many::<String>("param").ok().flatten() else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for kv in values {
        let (key, value) = kv.split_once('=').ok_or_else(|| {
            CliError::user(format!(
                "Invalid --param format: '{kv}'. Expected NAME=VALUE."
            ))
        })?;
        if key.is_empty() {
            return Err(CliError::user(format!(
                "Invalid --param format: '{kv}'. The name must not be empty."
            ))
            .into());
        }
        if !operation.parameters.iter().any(|p| p.name == key) {
            let known = operation
                .parameters
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>();
            let known = if known.is_empty() {
                "it takes none".to_string()
            } else {
                format!("known parameters: {}", known.join(", "))
            };
            return Err(CliError::user(format!(
                "Unknown --param '{key}' for operation '{}' ({known}).",
                operation.id
            ))
            .into());
        }
        out.push((key.to_string(), value.to_string()));
    }
    Ok(out)
}

fn upsert_query(query: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some(existing) = query.iter_mut().find(|(k, _)| k == key) {
        existing.1 = value.to_string();
    } else {
        query.push((key.to_string(), value.to_string()));
    }
}

fn extract_page_items(value: &Value) -> Vec<Value> {
    // Direct array
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    // Common wrapper fields
    for key in &["items", "results", "data"] {
        if let Some(arr) = value.get(key).and_then(|v| v.as_array()) {
            return arr.clone();
        }
    }
    // Any array field containing objects
    if let Some(obj) = value.as_object() {
        for (_, v) in obj {
            if let Some(arr) = v.as_array()
                && !arr.is_empty()
                && arr[0].is_object()
            {
                return arr.clone();
            }
        }
    }
    // Single item or non-array response
    vec![value.clone()]
}

fn build_body(
    operation: &Operation,
    args: &clap::ArgMatches,
    allow_prompting: bool,
) -> Result<Option<Value>> {
    // Use try_get_one to avoid panicking when args aren't registered
    // (operations without request body don't have --body/--body-file/--set)
    let body_arg = args.try_get_one::<String>("body").ok().flatten();
    let body_file_arg = args.try_get_one::<String>("body-file").ok().flatten();
    let set_args = args.try_get_many::<String>("set").ok().flatten();

    let has_explicit_body = body_arg.is_some()
        || body_file_arg.is_some()
        || set_args.as_ref().is_some_and(|v| v.len() > 0);

    // If no explicit body but operation has a schema, try interactive prompts
    if allow_prompting
        && !has_explicit_body
        && operation.has_request_body
        && let Some(ref schema) = operation.request_body_schema
        && let Some(body) =
            crate::interactive::prompt_for_body(schema, &operation.tag, &operation.action)?
    {
        return Ok(Some(body));
    }

    if let Some(body_str) = body_arg {
        if body_str == "-" {
            let stdin = std::io::read_to_string(std::io::stdin())?;
            let parsed: Value = serde_json::from_str(&stdin)
                .map_err(|e| CliError::user(format!("Invalid JSON from stdin: {e}")))?;
            return Ok(Some(parsed));
        }
        let parsed: Value = serde_json::from_str(body_str)
            .map_err(|e| CliError::user(format!("Invalid JSON in --body: {e}")))?;
        return Ok(Some(parsed));
    }

    if let Some(file_path) = body_file_arg {
        let content = std::fs::read_to_string(file_path)
            .map_err(|e| CliError::user(format!("Failed to read body file: {e}")))?;
        let parsed: Value = serde_json::from_str(&content)
            .map_err(|e| CliError::user(format!("Invalid JSON in body file: {e}")))?;
        return Ok(Some(parsed));
    }

    if let Some(values) = set_args {
        let values: Vec<&str> = values.map(String::as_str).collect();
        if !values.is_empty() {
            let mut body = serde_json::Map::new();
            for kv in values {
                let (key, val) = kv.split_once('=').ok_or_else(|| {
                    CliError::user(format!("Invalid --set format: {kv} (expected key=value)"))
                })?;
                set_nested(&mut body, key, coerce_value(val));
            }
            return Ok(Some(Value::Object(body)));
        }
    }

    if !operation.has_request_body {
        return Ok(None);
    }

    Ok(None)
}

fn set_nested(obj: &mut serde_json::Map<String, Value>, key: &str, value: Value) {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.len() == 1 {
        obj.insert(key.to_string(), value);
        return;
    }

    let mut current = obj;
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            current.insert(part.to_string(), value);
            return;
        }
        current = current
            .entry(part.to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .expect("Expected object for nested key");
    }
}
fn coerce_value(s: &str) -> Value {
    if s == "true" {
        return Value::Bool(true);
    }
    if s == "false" {
        return Value::Bool(false);
    }
    if s == "null" {
        return Value::Null;
    }
    if let Ok(n) = s.parse::<i64>() {
        return Value::Number(n.into());
    }
    if let Ok(n) = s.parse::<f64>()
        && let Some(num) = serde_json::Number::from_f64(n)
    {
        return Value::Number(num);
    }
    if ((s.starts_with('{') && s.ends_with('}')) || (s.starts_with('[') && s.ends_with(']')))
        && let Ok(v) = serde_json::from_str::<Value>(s)
    {
        return v;
    }
    Value::String(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_id_passes_through_untouched() {
        for id in ["42", "abc-123", "a.b_c~d", "USER99"] {
            assert_eq!(path_segment("id", id).unwrap(), id);
        }
    }

    #[test]
    fn a_separator_or_traversal_value_is_refused() {
        for bad in [
            "",
            ".",
            "..",
            "../users",
            "..",
            "1/2",
            "a\\b",
            "alerts/1/../../users",
        ] {
            assert!(
                path_segment("id", bad).is_err(),
                "'{bad}' must not be accepted as a path segment"
            );
        }
    }

    #[test]
    fn a_control_character_is_refused() {
        assert!(path_segment("id", "4\n2").is_err());
        assert!(path_segment("id", "4\r\n2").is_err());
        assert!(path_segment("id", "4\x002").is_err());
    }

    /// A pre-encoded separator must not decode back into one: the server has to
    /// see a literal `%2F` in the id, not a path boundary.
    #[test]
    fn a_pre_encoded_separator_is_encoded_again() {
        assert_eq!(path_segment("id", "%2F").unwrap(), "%252F");
        assert_eq!(path_segment("id", "..%2F..").unwrap(), "..%252F..");
    }

    #[test]
    fn other_reserved_characters_are_percent_encoded() {
        assert_eq!(path_segment("id", "a b").unwrap(), "a%20b");
        assert_eq!(path_segment("id", "a@b.com").unwrap(), "a%40b.com");
        assert_eq!(path_segment("id", "a?b#c").unwrap(), "a%3Fb%23c");
        // Multi-byte input encodes per UTF-8 byte.
        assert_eq!(path_segment("id", "ü").unwrap(), "%C3%BC");
    }

    #[test]
    fn the_error_names_the_parameter_and_the_value() {
        let err = path_segment("user-id", "../admin").unwrap_err().to_string();
        assert!(err.contains("user-id"));
        assert!(err.contains("../admin"));
    }
}
