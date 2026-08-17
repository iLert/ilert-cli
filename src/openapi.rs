use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::classification::{self, Classification};
use crate::config::ConfigManager;
use crate::endpoint::{Endpoint, validate_spec_path};
use crate::errors::CliError;

const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The spec fetch gets its own budget rather than inheriting reqwest's default
/// of none: it runs before the command tree exists, so a server that accepts
/// the connection and then stalls would hang the CLI with nothing on screen.
const SPEC_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of a spec we are willing to read.
///
/// The real document is around a megabyte. This is a bound on what a hostile or
/// broken endpoint can make us buffer — the body is decoded into memory and
/// then parsed into a `Value`, so an unbounded response is an unbounded
/// allocation. Generous enough that growth in the API cannot trip it.
const MAX_SPEC_BYTES: usize = 32 * 1024 * 1024;

/// The single cache file used before specs were kept per environment. Removed
/// opportunistically on the next write so an upgrade does not strand it.
const LEGACY_CACHE_FILE: &str = "openapi.json";

#[derive(Debug, Clone)]
pub struct Operation {
    pub id: String,
    pub method: String,
    pub path: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub tag: String,
    pub action: String,
    pub parameters: Vec<Parameter>,
    pub request_body_schema: Option<Value>,
    pub has_request_body: bool,
    /// `requestBody.required` from the spec. Kept so a missing body is a usage
    /// error we raise before previewing or sending, rather than a 400 from the
    /// server after the request has already gone out.
    pub request_body_required: bool,
    /// How dangerous this operation is. Resolved once, here, so the
    /// confirmation flow never has to guess from a method string.
    pub classification: Classification,
}

/// The query parameters that drive offset pagination. Both have to be present:
/// `--all` walks pages by rewriting them, so an operation carrying only one of
/// the two cannot be paged that way.
const OFFSET_PAGE_PARAMS: [&str; 2] = ["start-index", "max-results"];

impl Operation {
    pub fn query_param(&self, name: &str) -> Option<&Parameter> {
        self.parameters
            .iter()
            .find(|p| p.location == ParamLocation::Query && p.name == name)
    }

    /// Whether `--all` can actually walk this operation.
    ///
    /// Decided from the spec rather than from the shape of the path: most
    /// collection GETs page by offset, but a handful (`/alerts/count`,
    /// `/numbers`, the `/reports/*` endpoints, every `/users/{id}/contacts/*`
    /// list) return the whole set at once and declare neither parameter, and
    /// `/heartbeat-monitors` pages by `cursor` instead.
    pub fn supports_offset_pagination(&self) -> bool {
        OFFSET_PAGE_PARAMS
            .iter()
            .all(|name| self.query_param(name).is_some())
    }

    /// The server's own ceiling on `max-results`, when the spec states one.
    ///
    /// Caps differ per endpoint (20 on `/schedules`, 50 on `/status-pages`, 100
    /// on `/alerts`, 200 on `/heartbeat-monitors`) and overshooting is rejected
    /// with a `400` rather than clamped, so a fixed page size cannot be right
    /// everywhere.
    pub fn max_results_cap(&self) -> Option<u64> {
        self.query_param("max-results")?
            .schema
            .as_ref()?
            .get("maximum")?
            .as_u64()
    }
}

#[derive(Debug, Clone)]
pub struct Parameter {
    pub name: String,
    pub location: ParamLocation,
    pub required: bool,
    pub description: Option<String>,
    pub schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParamLocation {
    Path,
    Query,
    Header,
}

#[derive(Debug, Clone)]
pub struct OperationIndex {
    pub by_id: HashMap<String, Operation>,
    pub by_tag: HashMap<String, Vec<Operation>>,
}

impl OperationIndex {
    pub fn find_by_tag_action(&self, tag: &str, action: &str) -> Option<&Operation> {
        self.by_tag
            .get(tag)
            .and_then(|ops| ops.iter().find(|op| op.action == action))
    }

    pub fn actions_for_tag(&self, tag: &str) -> Vec<&str> {
        self.by_tag
            .get(tag)
            .map(|ops| ops.iter().map(|op| op.action.as_str()).collect())
            .unwrap_or_default()
    }
}

/// Load index from cache synchronously. Returns None if no cache exists.
/// Used at startup to build the dynamic command tree without network access.
pub fn load_cached_index(base_url: &str) -> Result<Option<OperationIndex>> {
    let cache_path = cache_file_path(base_url)?;
    match load_from_cache(&cache_path)? {
        Some(spec) => Ok(Some(build_index(&spec)?)),
        None => Ok(None),
    }
}

/// Ensure we have a fresh spec. Fetches if missing or stale, returns the index.
pub async fn ensure_spec(base_url: &str) -> Result<OperationIndex> {
    let cache_path = cache_file_path(base_url)?;
    // Resolved through the same gate as every other request rather than
    // concatenated: the spec decides which commands exist, so fetching it from
    // the wrong origin is worse than sending one request there.
    let spec_url = Endpoint::parse(base_url)?.resolve("/api-docs/openapi.json")?;

    // Try cache first
    if let Some(cached) = load_from_cache(&cache_path)? {
        if !is_cache_stale(&cache_path)? {
            return build_index(&cached);
        }
        // Stale — try refresh, fall back to stale
        eprintln!("Note: API spec cache is stale, refreshing...");
        if let Ok(fresh) = fetch_spec(&spec_url).await {
            save_to_cache(&cache_path, &fresh)?;
            return build_index(&fresh);
        }
        eprintln!("Warning: Could not refresh API spec, using cached version.");
        return build_index(&cached);
    }

    // No cache — must fetch
    let spec = fetch_spec(&spec_url)
        .await
        .context("Failed to fetch API spec. Check your network connection.")?;
    save_to_cache(&cache_path, &spec)?;
    build_index(&spec)
}

/// Fetch and parse the OpenAPI document.
///
/// Unlike an API call this one is unauthenticated, but it is not low-stakes:
/// the document it returns becomes the command tree, the request paths and the
/// destructive/read-only classification of everything the CLI can do. So it is
/// fetched under the same rules as the rest — no redirects (a 302 would let the
/// endpoint hand spec authority to a host the profile never named), a timeout,
/// an explicit status check, and a ceiling on how much we will read.
pub(crate) async fn fetch_spec(url: &url::Url) -> Result<Value> {
    let client = crate::client::builder()
        .timeout(SPEC_FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let mut response = client.get(url.clone()).send().await?;

    let status = response.status();
    if !status.is_success() {
        // Report the redirect rather than letting it look like a server error:
        // "the spec moved" is a different problem from "the spec is broken".
        let hint = if status.is_redirection() {
            " (redirects are not followed for the API spec)"
        } else {
            ""
        };
        return Err(CliError::user(format!(
            "Failed to fetch the API spec from {url}: HTTP {}{hint}.",
            status.as_u16()
        ))
        .into());
    }

    // `Content-Length` is a hint, not a promise, so it is used to fail early
    // and the running total below is what actually enforces the limit.
    if let Some(declared) = response.content_length()
        && declared > MAX_SPEC_BYTES as u64
    {
        return Err(spec_too_large(url));
    }

    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_SPEC_BYTES {
            return Err(spec_too_large(url));
        }
        body.extend_from_slice(&chunk);
    }

    serde_json::from_slice(&body)
        .map_err(|e| CliError::user(format!("The API spec at {url} is not valid JSON: {e}")).into())
}

fn spec_too_large(url: &url::Url) -> anyhow::Error {
    CliError::user(format!(
        "The API spec at {url} is larger than the {} MiB limit; refusing to read it.",
        MAX_SPEC_BYTES / (1024 * 1024)
    ))
    .into()
}

/// Where the spec served by `base_url` is cached.
///
/// One file per environment. The cached spec decides which commands exist and
/// what they classify as, so serving a staging spec to a production profile
/// (or the reverse) just because it was fetched more recently would be wrong in
/// a way that is invisible at the call site.
pub(crate) fn cache_file_path(base_url: &str) -> Result<PathBuf> {
    let cache_dir = ConfigManager::cache_dir()?;
    Ok(cache_dir.join(format!("openapi-{}.json", cache_key(base_url))))
}

/// A filesystem-safe, stable name for an environment: a readable slug of the
/// host so the cache directory can be understood at a glance, plus a digest so
/// two URLs that slugify the same never share a file.
fn cache_key(base_url: &str) -> String {
    let normalized = base_url.trim_end_matches('/').to_ascii_lowercase();
    let bare = normalized
        .strip_prefix("https://")
        .or_else(|| normalized.strip_prefix("http://"))
        .unwrap_or(&normalized);
    let slug: String = bare
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    format!("{slug}-{:08x}", fnv1a(&normalized))
}

/// FNV-1a, hand-rolled: the standard library's hasher makes no promise of
/// stability across releases, and a cache filename that changes with the
/// toolchain would silently orphan every spec already on disk.
fn fnv1a(value: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in value.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn load_from_cache(path: &PathBuf) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path).context("Failed to read cached spec")?;
    let spec: Value = serde_json::from_str(&content).context("Failed to parse cached spec")?;
    Ok(Some(spec))
}

fn is_cache_stale(path: &PathBuf) -> Result<bool> {
    let metadata = std::fs::metadata(path)?;
    let modified = metadata.modified()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::MAX);
    Ok(age > CACHE_TTL)
}

pub(crate) fn save_to_cache(path: &PathBuf, spec: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        // Nothing reads the pre-per-environment file any more; drop it rather
        // than leave a megabyte of unreachable spec behind. Best effort — a
        // failure here has no bearing on the write that matters.
        let _ = std::fs::remove_file(parent.join(LEGACY_CACHE_FILE));
    }
    let content = serde_json::to_string(spec)?;
    std::fs::write(path, content)?;
    Ok(())
}

fn build_index(spec: &Value) -> Result<OperationIndex> {
    let paths = spec
        .get("paths")
        .and_then(|v| v.as_object())
        .ok_or_else(|| CliError::user("Invalid OpenAPI spec: missing 'paths'"))?;

    // Extract server base path (e.g., "/api" from servers[0].url)
    let server_base = spec
        .get("servers")
        .and_then(|s| s.as_array())
        .and_then(|a| a.first())
        .and_then(|s| s.get("url"))
        .and_then(|u| u.as_str())
        .and_then(|u| {
            // Only use path-only server URLs (relative paths like "/api")
            // Skip full URLs like "https://api.ilert.com/api"
            if u.starts_with('/') {
                Some(u.trim_end_matches('/'))
            } else {
                None
            }
        })
        .unwrap_or("");

    // `servers[0].url` is prepended to every path, so a hostile one poisons the
    // whole document at once — `"//evil.example"` would turn each path into a
    // scheme-relative URL. Checked on its own so the failure is reported once
    // and names the real culprit, instead of once per path.
    let server_base = match crate::endpoint::validate_request_path(server_base) {
        _ if server_base.is_empty() => "",
        Ok(()) => server_base,
        Err(e) => {
            // The warning quotes the very string that failed validation, which
            // is the string most likely to be hostile — a rejected path that
            // clears the screen on its way to being reported would hide the
            // report.
            eprintln!(
                "Warning: ignoring API spec server base path '{}': {}",
                crate::sanitize::terminal_text(server_base),
                crate::sanitize::terminal_string(e.to_string())
            );
            ""
        }
    };

    let mut by_id: HashMap<String, Operation> = HashMap::new();
    let mut by_tag: HashMap<String, Vec<Operation>> = HashMap::new();
    let mut action_counts: HashMap<String, HashMap<String, usize>> = HashMap::new();

    for (path, methods) in paths {
        let full_path = format!("{server_base}{path}");
        // Checked here so a path that could re-target a request never becomes a
        // command at all — `ilert alerts list` cannot be made to send the
        // caller's token to `//evil.example` if the operation does not exist.
        // Dropped rather than fatal: one malformed key in a spec we did not
        // write should not take the whole CLI down with it, and
        // `HttpClient::request_raw` refuses the same path again anyway.
        if let Err(e) = validate_spec_path(&full_path) {
            eprintln!(
                "Warning: ignoring API spec path '{}': {}",
                crate::sanitize::terminal_text(path),
                crate::sanitize::terminal_string(e.to_string())
            );
            continue;
        }
        let methods = match methods.as_object() {
            Some(m) => m,
            None => continue,
        };

        for (method, op_value) in methods {
            if !matches!(
                method.as_str(),
                "get" | "head" | "post" | "put" | "patch" | "delete"
            ) {
                continue;
            }

            let tag = op_value
                .get("tags")
                .and_then(|t| t.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("other")
                .to_string();

            let tag_normalized = normalize_tag(&tag);
            let action = derive_action(method, path);

            let operation_id = operation_id(method, path, op_value);

            let parameters = extract_parameters(op_value);
            let request_body = op_value.get("requestBody");
            let has_request_body = request_body.is_some();
            let request_body_required = request_body
                .and_then(|rb| rb.get("required"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let request_body_schema = op_value
                .get("requestBody")
                .and_then(|rb| rb.get("content"))
                .and_then(|c| c.get("application/json"))
                .and_then(|j| j.get("schema"))
                .map(|s| resolve_refs(s, spec));

            let op = Operation {
                id: operation_id.clone(),
                method: method.to_uppercase(),
                path: full_path.clone(),
                summary: op_value
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                description: op_value
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                tag: tag_normalized.clone(),
                action: action.clone(),
                parameters,
                request_body_schema,
                has_request_body,
                request_body_required,
                classification: classification::for_operation(method, &operation_id, op_value)?,
            };

            // Track action counts for collision handling
            *action_counts
                .entry(tag_normalized.clone())
                .or_default()
                .entry(action.clone())
                .or_insert(0) += 1;

            by_id.insert(operation_id, op.clone());
            by_tag.entry(tag_normalized).or_default().push(op);
        }
    }

    // Resolve action name collisions within tags
    for (tag, ops) in &mut by_tag {
        let counts = action_counts.get(tag.as_str()).cloned().unwrap_or_default();
        let mut seen: HashMap<String, usize> = HashMap::new();

        for op in ops.iter_mut() {
            if let Some(&count) = counts.get(&op.action)
                && count > 1
            {
                let idx = seen.entry(op.action.clone()).or_insert(0);
                if *idx > 0 {
                    // Append path tail for disambiguation
                    let suffix = path_suffix(&op.path);
                    op.action = format!("{}-{suffix}", op.action);
                }
                *idx += 1;
            }
        }
    }

    Ok(OperationIndex { by_id, by_tag })
}

fn derive_action(method: &str, path: &str) -> String {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let ends_with_param = segments
        .last()
        .is_some_and(|s| s.starts_with('{') && s.ends_with('}'));

    match (method, ends_with_param) {
        ("get", false) => "list".to_string(),
        ("get", true) => "get".to_string(),
        ("post", _) => "create".to_string(),
        ("put", _) => "update".to_string(),
        ("patch", _) => "patch".to_string(),
        ("delete", _) => "delete".to_string(),
        _ => method.to_string(),
    }
}

/// A spec `tag` as the CLI names it: a command, an index key and a line of
/// `--help`, all at once.
///
/// Sanitized here rather than where it is printed so all three agree. Escaping
/// only at print time would leave `index.by_tag` keyed by a string carrying an
/// escape sequence while the command was named by the escaped one, and dispatch
/// would stop finding it.
fn normalize_tag(tag: &str) -> String {
    crate::sanitize::terminal_string(tag.to_lowercase().replace([' ', '_'], "-"))
}

/// The identity an operation is known by: its `operationId` when the spec
/// declares one, otherwise `{method}-{slugified-path}`.
///
/// The current ilert spec declares none, so in practice every id is synthesized
/// — which makes this the key `classification::OPERATION_OVERRIDES` is written
/// against. It is public so the classification tests resolve ids the same way
/// the index does, rather than reimplementing the fallback and drifting from it.
pub fn operation_id(method: &str, path: &str, op_value: &Value) -> String {
    op_value
        .get("operationId")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("{method}-{}", slugify_path(path)))
}

fn slugify_path(path: &str) -> String {
    path.trim_matches('/')
        .replace('/', "-")
        .replace(['{', '}'], "")
}

/// The tail segment a colliding action is disambiguated by. Part of a command
/// name, so it is escaped for the same reason [`normalize_tag`] is.
fn path_suffix(path: &str) -> String {
    let segments: Vec<&str> = path
        .split('/')
        .filter(|s| !s.is_empty() && !s.starts_with('{'))
        .collect();
    crate::sanitize::terminal_string(segments.last().unwrap_or(&"unknown").to_lowercase())
}

fn extract_parameters(op: &Value) -> Vec<Parameter> {
    let params = match op.get("parameters").and_then(|v| v.as_array()) {
        Some(p) => p,
        None => return Vec::new(),
    };

    params
        .iter()
        .filter_map(|p| {
            // A parameter name becomes a `--flag`, a lookup key in
            // `RequestParams::from_operation`, and a query-string key on the
            // wire. Escaping it here keeps all three the same string.
            let name = crate::sanitize::terminal_text(p.get("name")?.as_str()?);
            let location = match p.get("in")?.as_str()? {
                "path" => ParamLocation::Path,
                "query" => ParamLocation::Query,
                "header" => ParamLocation::Header,
                _ => return None,
            };
            let required = p.get("required").and_then(|v| v.as_bool()).unwrap_or(false);
            let description = p
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from);
            let schema = p.get("schema").cloned();

            Some(Parameter {
                name,
                location,
                required,
                description,
                schema,
            })
        })
        .collect()
}

/// Resolve `$ref` pointers in a JSON schema against the full OpenAPI spec.
/// Handles `$ref`, `allOf`, and nested property refs. Limits depth to prevent cycles.
fn resolve_refs(schema: &Value, spec: &Value) -> Value {
    resolve_refs_depth(schema, spec, 10)
}

fn resolve_refs_depth(schema: &Value, spec: &Value, depth: u32) -> Value {
    if depth == 0 {
        return schema.clone();
    }

    // Direct $ref
    if let Some(ref_path) = schema.get("$ref").and_then(|v| v.as_str()) {
        if let Some(resolved) = follow_ref(ref_path, spec) {
            return resolve_refs_depth(resolved, spec, depth - 1);
        }
        return schema.clone();
    }

    // allOf — merge all items into a single schema
    if let Some(all_of) = schema.get("allOf").and_then(|v| v.as_array()) {
        let mut merged = serde_json::Map::new();
        let mut merged_props = serde_json::Map::new();
        let mut merged_required: Vec<Value> = Vec::new();

        for item in all_of {
            let resolved_item = resolve_refs_depth(item, spec, depth - 1);
            if let Some(obj) = resolved_item.as_object() {
                if let Some(props) = obj.get("properties").and_then(|v| v.as_object()) {
                    for (k, v) in props {
                        merged_props.insert(k.clone(), resolve_refs_depth(v, spec, depth - 1));
                    }
                }
                if let Some(req) = obj.get("required").and_then(|v| v.as_array()) {
                    merged_required.extend(req.iter().cloned());
                }
                // Copy other fields (type, description, etc.)
                for (k, v) in obj {
                    if k != "properties" && k != "required" && k != "allOf" {
                        merged.insert(k.clone(), v.clone());
                    }
                }
            }
        }

        if !merged_props.is_empty() {
            merged.insert("properties".to_string(), Value::Object(merged_props));
        }
        if !merged_required.is_empty() {
            merged.insert("required".to_string(), Value::Array(merged_required));
        }
        merged.insert("type".to_string(), Value::String("object".to_string()));

        return Value::Object(merged);
    }

    // Resolve refs inside properties
    if let Some(obj) = schema.as_object() {
        let mut result = obj.clone();
        if let Some(props) = obj.get("properties").and_then(|v| v.as_object()) {
            let mut resolved_props = serde_json::Map::new();
            for (k, v) in props {
                resolved_props.insert(k.clone(), resolve_refs_depth(v, spec, depth - 1));
            }
            result.insert("properties".to_string(), Value::Object(resolved_props));
        }
        return Value::Object(result);
    }

    schema.clone()
}

/// Follow a JSON Pointer-style $ref like "#/components/schemas/Event".
fn follow_ref<'a>(ref_path: &str, spec: &'a Value) -> Option<&'a Value> {
    let path = ref_path.strip_prefix("#/")?;
    let mut current = spec;
    for segment in path.split('/') {
        current = current.get(segment)?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::cache_key;

    #[test]
    fn environments_get_separate_cache_keys() {
        assert_ne!(
            cache_key("https://api.ilert.com"),
            cache_key("https://api.ilert.dev")
        );
    }

    #[test]
    fn cache_key_ignores_trailing_slash_and_case() {
        let canonical = cache_key("https://api.ilert.com");
        assert_eq!(cache_key("https://api.ilert.com/"), canonical);
        assert_eq!(cache_key("https://API.ilert.com"), canonical);
    }

    #[test]
    fn cache_key_is_a_safe_file_name() {
        let key = cache_key("http://localhost:8080/gateway");
        assert!(
            key.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'),
            "unsafe cache key: {key}"
        );
        assert!(key.starts_with("localhost_8080_gateway-"), "got: {key}");
    }

    #[test]
    fn long_urls_stay_distinct_after_truncation() {
        // The readable slug is capped, so only the digest separates URLs that
        // share a long prefix.
        let prefix = "https://very-long-host-name-that-exceeds-the-slug-budget.example.com";
        assert_ne!(
            cache_key(&format!("{prefix}/alpha")),
            cache_key(&format!("{prefix}/beta"))
        );
    }

    mod pagination {
        use crate::openapi::{Classification, Operation, ParamLocation, Parameter};

        fn query_param(name: &str, schema: Option<serde_json::Value>) -> Parameter {
            Parameter {
                name: name.to_string(),
                location: ParamLocation::Query,
                required: false,
                description: None,
                schema,
            }
        }

        fn operation(parameters: Vec<Parameter>) -> Operation {
            Operation {
                id: "get-things".into(),
                method: "GET".into(),
                path: "/things".into(),
                summary: None,
                description: None,
                tag: "things".into(),
                action: "list".into(),
                parameters,
                request_body_schema: None,
                has_request_body: false,
                request_body_required: false,
                classification: Classification::from_method("GET"),
            }
        }

        #[test]
        fn offset_paging_needs_both_parameters() {
            let both = operation(vec![
                query_param("start-index", None),
                query_param("max-results", None),
            ]);
            assert!(both.supports_offset_pagination());

            // `/heartbeat-monitors` pages by cursor: max-results, no start-index.
            let cursor = operation(vec![
                query_param("max-results", None),
                query_param("cursor", None),
            ]);
            assert!(!cursor.supports_offset_pagination());

            // `/numbers`, `/reports/*` and friends declare neither.
            assert!(!operation(vec![]).supports_offset_pagination());
        }

        #[test]
        fn a_declared_maximum_is_the_page_cap() {
            let capped = operation(vec![query_param(
                "max-results",
                Some(serde_json::json!({"default": 20, "maximum": 20})),
            )]);
            assert_eq!(capped.max_results_cap(), Some(20));

            let uncapped = operation(vec![query_param(
                "max-results",
                Some(serde_json::json!({"default": 50})),
            )]);
            assert_eq!(uncapped.max_results_cap(), None);
            assert_eq!(operation(vec![]).max_results_cap(), None);
        }
    }
}
