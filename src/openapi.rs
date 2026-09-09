use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use colored::Colorize;
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

/// Where each OpenAPI document lives, relative to the API base URL.
///
/// ilert publishes two: the stable API, and a companion document describing the
/// beta endpoints, released and versioned separately. They are merged into one
/// before anything downstream sees them, so a beta endpoint arrives as an
/// ordinary command rather than as a mode the caller has to know about.
const STABLE_SPEC_PATH: &str = "/api-docs/openapi.json";
const BETA_SPEC_PATH: &str = "/api/beta/openapi.json";

/// Set to a non-empty value to leave the beta document out entirely — the
/// escape hatch for a beta endpoint that breaks the command tree. It gates the
/// fetch, so it takes effect on the next one: `ilert config cache refresh`
/// applies it immediately, and otherwise the cached merged spec stands until
/// its TTL.
const NO_BETA_ENV: &str = "ILERT_NO_BETA_SPEC";

/// Stamped onto every operation the beta document contributed, so the merge is
/// still legible in the cached document — which is the only place the two
/// specs can still be told apart once they are one.
const SPEC_ORIGIN_KEY: &str = "x-ilert-cli-spec";
const BETA_ORIGIN: &str = "beta";

/// Stamped onto every operation with the server base path its own document
/// declared.
///
/// Two specs can state two different prefixes, and one `servers` array cannot
/// hold both. Recording it per operation keeps the `paths` keys exactly as each
/// document authored them — which matters because [`operation_id`] synthesizes
/// ids from those keys, and every `OPERATION_OVERRIDES` entry, every
/// `ilert ops run <id>` and every classification test is written against the
/// ids the unprefixed keys produce.
const SPEC_BASE_PATH_KEY: &str = "x-ilert-cli-base-path";

/// Where the merged document records the version of each document it was built
/// from. `info.version` stays the stable one, since that is the version the API
/// is known by.
const SPEC_VERSIONS_KEY: &str = "x-ilert-cli-spec-versions";

/// The prefix a beta name is moved under when it would otherwise take one that
/// belongs to the stable half of the document.
///
/// Two of those namespaces exist, for the same reason. Both documents define a
/// `Postmortem` schema and they are not the same shape, so without a namespace
/// whichever landed last would silently rewrite the `--set` help and the
/// dry-run preview of the *other* document's commands. And `by_id` is what
/// `ops run <id>` dispatches on, so a beta operation declaring an
/// `operationId` the stable spec already uses would replace it outright.
const BETA_NAMESPACE: &str = "beta.";

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
    /// Whether this operation came from the beta document. It behaves like any
    /// other — the flag exists so `ops list` and `ops show` can say so, and so
    /// the name a collision resolves to is decided stable-first.
    pub beta: bool,
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

impl Parameter {
    /// Whether the spec declares this parameter as a list of values.
    ///
    /// Every array parameter in the ilert spec is `style: form, explode: true`,
    /// which is also OpenAPI's default for query parameters, so a list travels
    /// as one `name=value` pair per element rather than one comma-joined pair.
    pub fn is_list(&self) -> bool {
        self.schema
            .as_ref()
            .and_then(|schema| schema.get("type"))
            .and_then(|ty| ty.as_str())
            == Some("array")
    }

    /// The value to send when the caller supplies none.
    ///
    /// Only consulted for a *required* parameter. An optional one is left out
    /// of the request entirely, which is what the server's own default is for —
    /// sending it explicitly would state a choice the caller never made.
    ///
    /// A required parameter with a fixed default is the shape the beta document
    /// introduces: every beta operation declares an `ilert-beta` opt-in header
    /// whose value is not interpreted. Filling it in is what keeps a beta
    /// command the same amount of typing as a stable one; `--ilert-beta` is
    /// still there for anyone who needs to send something else.
    pub fn default_value(&self) -> Option<String> {
        match self.schema.as_ref()?.get("default")? {
            Value::String(s) => Some(crate::sanitize::terminal_text(s)),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
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
    load_index_from_cache(&cache_file_path(base_url)?)
}

/// Ensure we have a fresh spec. Fetches if missing or stale, returns the index.
pub async fn ensure_spec(base_url: &str) -> Result<OperationIndex> {
    let cache_path = cache_file_path(base_url)?;
    // Validated here rather than only on the fetch path, so a base URL the CLI
    // would refuse to send anything to is refused the same way whether or not
    // this environment happens to have a cached spec.
    Endpoint::parse(base_url)?;

    // Try cache first. An unusable one reads as absent, so it falls through to
    // the fetch below and is replaced rather than becoming an error.
    if let Some(cached) = load_index_from_cache(&cache_path)? {
        if !is_cache_stale(&cache_path)? {
            return Ok(cached);
        }
        // Stale — try refresh, fall back to stale
        eprintln!("Note: API spec cache is stale, refreshing...");
        if let Ok(fresh) = fetch_merged_spec(base_url).await {
            save_to_cache(&cache_path, &fresh)?;
            return build_index(&fresh);
        }
        eprintln!("Warning: Could not refresh API spec, using cached version.");
        return Ok(cached);
    }

    // No cache, or none we can use — must fetch
    let spec = fetch_merged_spec(base_url)
        .await
        .context("Failed to fetch API spec. Check your network connection.")?;
    save_to_cache(&cache_path, &spec)?;
    build_index(&spec)
}

/// The document the rest of the CLI works from: the stable spec with the beta
/// spec merged into it.
///
/// The stable document is what the CLI is; failing to fetch it is a failure.
/// The beta one is additive, so a deployment that does not serve it answers
/// `404` and that is not an error — it is an environment with no beta
/// endpoints, which is exactly what a self-hosted or older ilert is. Any other
/// failure is reported once and then treated the same way, because a
/// stable-only CLI still does everything it did before beta existed.
pub(crate) async fn fetch_merged_spec(base_url: &str) -> Result<Value> {
    // Both URLs are resolved through the same gate as every other request
    // rather than concatenated: the spec decides which commands exist, so
    // fetching it from the wrong origin is worse than sending one request there.
    let endpoint = Endpoint::parse(base_url)?;
    let stable = fetch_spec(&endpoint.resolve(STABLE_SPEC_PATH)?).await?;
    let beta = fetch_beta_spec(&endpoint).await;
    Ok(merge_specs(stable, beta))
}

/// The beta document, or nothing — this never fails the command that needed it.
async fn fetch_beta_spec(endpoint: &Endpoint) -> Option<Value> {
    if std::env::var_os(NO_BETA_ENV).is_some_and(|value| !value.is_empty()) {
        return None;
    }

    let url = endpoint.resolve(BETA_SPEC_PATH).ok()?;
    match fetch_optional_spec(&url).await {
        Ok(spec) => spec,
        Err(e) => {
            eprintln!(
                "{} Could not fetch the beta API spec from {url}: {} Beta endpoints \
                 will not be available.",
                "Warning:".yellow().bold(),
                crate::sanitize::terminal_string(e.to_string()),
            );
            None
        }
    }
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
    let spec = fetch_spec_inner(url, false).await?;
    Ok(spec.expect("only a fetch allowed to come back empty can come back empty"))
}

/// [`fetch_spec`], except that a `404` reads as "this deployment does not serve
/// that document" rather than as a failure. For the beta spec, which not every
/// ilert publishes.
async fn fetch_optional_spec(url: &url::Url) -> Result<Option<Value>> {
    fetch_spec_inner(url, true).await
}

async fn fetch_spec_inner(url: &url::Url, missing_on_404: bool) -> Result<Option<Value>> {
    let client = crate::client::builder()
        .timeout(SPEC_FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let mut response = client.get(url.clone()).send().await?;

    let status = response.status();
    if missing_on_404 && status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
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
        .map(Some)
        .map_err(|e| CliError::user(format!("The API spec at {url} is not valid JSON: {e}")).into())
}

/// Fold the beta document into the stable one, producing the single OpenAPI
/// document everything downstream consumes: the cache file, the index, the
/// command tree, `ops`, the preview.
///
/// A pure `Value -> Value` step slotted between the fetch and the cache write
/// is the whole design: nothing after it has to learn that there were ever two
/// documents. What it has to get right is the handful of places where two
/// specs collide — see the four helpers below.
pub(crate) fn merge_specs(stable: Value, beta: Option<Value>) -> Value {
    let mut merged = stable;
    if !merged.is_object() {
        // Not a document at all. Left exactly as it arrived so `build_index`
        // is the one place that reports what is wrong with it.
        return merged;
    }

    let mut versions = serde_json::Map::new();
    if let Some(version) = spec_version_of(&merged) {
        versions.insert("stable".to_string(), Value::String(version));
    }

    stamp_base_path(&mut merged);

    if let Some(mut beta) = beta.filter(Value::is_object) {
        // Recorded only on success. A beta fetch that failed leaves the key
        // absent, so the next freshness check sees a difference and refetches
        // rather than sitting on a spec-minus-beta for the whole TTL.
        if let Some(version) = spec_version_of(&beta) {
            versions.insert("beta".to_string(), Value::String(version));
        }
        stamp_base_path(&mut beta);
        namespace_components(&mut beta);
        stamp_beta_operations(&mut beta);
        report_taken_operation_ids(&merged, &beta);
        merge_paths(&mut merged, &beta);
        merge_components(&mut merged, &beta);
    }

    merged[SPEC_VERSIONS_KEY] = Value::Object(versions);
    merged
}

/// Move the document's server base path onto its operations and empty
/// `servers`.
///
/// `build_index` prepends `servers[0].url` to every path, and two documents can
/// state two different prefixes — so the prefix has to travel with the
/// operation rather than with the document. Done for the stable document too,
/// even when there is no beta one, so there is a single code path.
///
/// The `paths` keys are deliberately left as authored: [`operation_id`]
/// synthesizes ids from them, and rewriting the keys would rename every
/// operation in the CLI.
fn stamp_base_path(spec: &mut Value) {
    let prefix = server_prefix(spec);
    for (_, _, op) in operations_mut(spec) {
        op.insert(
            SPEC_BASE_PATH_KEY.to_string(),
            Value::String(prefix.clone()),
        );
    }
    spec["servers"] = Value::Array(Vec::new());
}

/// Mark every beta operation as one.
fn stamp_beta_operations(beta: &mut Value) {
    for (_, _, op) in operations_mut(beta) {
        op.insert(
            SPEC_ORIGIN_KEY.to_string(),
            Value::String(BETA_ORIGIN.to_string()),
        );
    }
}

/// Rename everything the beta document defines under `components`, and rewrite
/// every `$ref` in it to match.
///
/// `resolve_refs` follows `#/components/schemas/X` against the whole merged
/// document, so two definitions of one name cannot coexist there — the second
/// does not lose an argument with the first, it replaces it, and the damage
/// lands on the *stable* command that pointed at the original. Namespacing
/// makes that class of bug impossible rather than merely unlikely.
fn namespace_components(beta: &mut Value) {
    namespace_refs(beta);

    for section in COMPONENT_SECTIONS {
        let Some(components) = beta
            .get_mut("components")
            .and_then(|c| c.get_mut(section))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        *components = components
            .iter()
            .map(|(name, value)| (format!("{BETA_NAMESPACE}{name}"), value.clone()))
            .collect();
    }
}

/// The `components` sections a `$ref` in either document can point into. The
/// specs use no others; one that appeared would keep its own name and simply
/// not be namespaced, which is the same position we were in before.
const COMPONENT_SECTIONS: [&str; 2] = ["schemas", "parameters"];

fn namespace_refs(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if key == "$ref"
                    && let Some(target) = child.as_str()
                    && let Some(renamed) = namespaced_ref(target)
                {
                    *child = Value::String(renamed);
                    continue;
                }
                namespace_refs(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(namespace_refs),
        _ => {}
    }
}

/// `#/components/schemas/Postmortem` -> `#/components/schemas/beta.Postmortem`.
///
/// Only the component's own name is prefixed, so a pointer that continues into
/// the definition (`.../Postmortem/properties/id`) still lands where it did.
fn namespaced_ref(target: &str) -> Option<String> {
    COMPONENT_SECTIONS.iter().find_map(|section| {
        let head = format!("#/components/{section}/");
        let rest = target.strip_prefix(&head)?;
        Some(format!("{head}{BETA_NAMESPACE}{rest}"))
    })
}

/// Merge the beta document's paths in, one method at a time.
///
/// Per `(path, method)` rather than per path: a beta document that adds
/// `POST /x` to a path the stable one already serves as `GET /x` must not take
/// the stable `GET` with it. A genuine conflict — the same method on the same
/// endpoint in both — is resolved in the stable document's favour, since that
/// is the contract the CLI has been shipping.
///
/// A shared `paths` key is not by itself a shared endpoint. Each document
/// states its own server prefix, so a stable `/incidents` under `/api` and a
/// beta `/incidents` under `/api/beta` are two different endpoints that happen
/// to be written the same way. Treating that as a conflict would drop the beta
/// endpoint on the floor, so it is re-keyed by its full path instead — which is
/// what the key already is for every document whose prefix matches.
fn merge_paths(merged: &mut Value, beta: &Value) {
    let Some(beta_paths) = beta.get("paths").and_then(Value::as_object) else {
        return;
    };
    let Some(paths) = ensure_object(merged, &["paths"]) else {
        return;
    };

    for (path, beta_item) in beta_paths {
        let Some(beta_item) = beta_item.as_object() else {
            continue;
        };
        // Resolved before the borrow of `paths` that the merge below needs, so
        // the decision and the write do not overlap.
        let existing_base = paths
            .get(path)
            .and_then(Value::as_object)
            .map(item_base_path);
        let beta_base = item_base_path(beta_item);

        match existing_base {
            None => {
                paths.insert(path.clone(), Value::Object(beta_item.clone()));
                continue;
            }
            // Same key, different endpoint.
            Some(existing_base) if existing_base != beta_base => {
                let key = format!("{beta_base}{path}");
                if paths.contains_key(&key) {
                    eprintln!(
                        "Warning: the beta API spec describes {} under a prefix the stable \
                         spec already uses; dropping it.",
                        crate::sanitize::terminal_text(&key),
                    );
                    continue;
                }
                // The key now carries the prefix, so the operations must not
                // carry it a second time.
                paths.insert(key, rebased_item(beta_item));
                continue;
            }
            Some(_) => {}
        }

        let Some(existing) = paths.get_mut(path).and_then(Value::as_object_mut) else {
            continue;
        };
        for (method, operation) in beta_item {
            // Only operations are carried over. A path-item level key on a path
            // both documents describe (`parameters`, `summary`) would apply to
            // the stable operations too, which is not what the beta document
            // said.
            if !is_http_method(method) {
                continue;
            }
            if existing.contains_key(method) {
                eprintln!(
                    "Warning: the beta API spec redefines {} {}; keeping the stable definition.",
                    method.to_uppercase(),
                    crate::sanitize::terminal_text(path),
                );
                continue;
            }
            existing.insert(method.clone(), operation.clone());
        }
    }
}

/// Say so when the beta document asks for an operation id that already means
/// something.
///
/// [`build_index`] namespaces it either way — that is the enforcement, and it
/// runs against whatever document is on disk. This is the line that says it
/// happened, printed where a spec is fetched rather than on every command.
fn report_taken_operation_ids(merged: &Value, beta: &Value) {
    let reserved = reserved_operation_ids(merged);
    for (path, method, op_value) in operations(beta) {
        let id = operation_id(method, path, op_value);
        if reserved.contains(&id) {
            eprintln!(
                "Warning: the beta API spec claims the operation id '{}', which already \
                 belongs to the stable API; it is indexed under a '{BETA_NAMESPACE}' prefix.",
                crate::sanitize::terminal_text(&id),
            );
        }
    }
}

/// The server prefix the operations of a path item were stamped with, or `""`.
///
/// Read off the first operation: every operation in a document is stamped with
/// that document's prefix, and a path item only ever holds operations from one
/// document — [`merge_paths`] re-keys the item rather than mixing two.
fn item_base_path(item: &serde_json::Map<String, Value>) -> String {
    item.iter()
        .filter(|(method, _)| is_http_method(method))
        .find_map(|(_, op)| op.get(SPEC_BASE_PATH_KEY)?.as_str())
        .unwrap_or_default()
        .to_string()
}

/// A path item whose key has taken over its prefix: the operations are stamped
/// with an empty one, so `build_index` prepends nothing to a key that is
/// already the full path.
fn rebased_item(item: &serde_json::Map<String, Value>) -> Value {
    let mut item = item.clone();
    for (method, operation) in item.iter_mut() {
        if !is_http_method(method) {
            continue;
        }
        if let Some(operation) = operation.as_object_mut() {
            operation.insert(SPEC_BASE_PATH_KEY.to_string(), Value::String(String::new()));
        }
    }
    Value::Object(item)
}

/// Merge the beta document's components in. Its names are namespaced by now, so
/// a collision here would mean the stable document itself defines a `beta.`
/// name — in which case the one that was already there wins, like everywhere
/// else in this merge.
fn merge_components(merged: &mut Value, beta: &Value) {
    for section in COMPONENT_SECTIONS {
        let Some(beta_section) = beta
            .get("components")
            .and_then(|c| c.get(section))
            .and_then(Value::as_object)
        else {
            continue;
        };
        if beta_section.is_empty() {
            continue;
        }
        let Some(target) = ensure_object(merged, &["components", section]) else {
            continue;
        };
        for (name, definition) in beta_section {
            if !target.contains_key(name) {
                target.insert(name.clone(), definition.clone());
            }
        }
    }
}

/// Every operation in a document, as `(path, method, operation)`.
fn operations(spec: &Value) -> impl Iterator<Item = (&String, &String, &Value)> {
    spec.get("paths")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|paths| {
            paths.iter().flat_map(|(path, item)| {
                item.as_object()
                    .into_iter()
                    .flat_map(move |methods| methods.iter().map(move |(m, op)| (path, m, op)))
            })
        })
        .filter(|(_, method, _)| is_http_method(method))
}

/// Every operation object in a document, as `(path, method, operation)`.
fn operations_mut(
    spec: &mut Value,
) -> impl Iterator<Item = (String, String, &mut serde_json::Map<String, Value>)> {
    spec.get_mut("paths")
        .and_then(Value::as_object_mut)
        .into_iter()
        .flat_map(|paths| {
            paths.iter_mut().flat_map(|(path, item)| {
                let path = path.clone();
                item.as_object_mut().into_iter().flat_map(move |methods| {
                    let path = path.clone();
                    methods
                        .iter_mut()
                        .filter(|(method, _)| is_http_method(method))
                        .filter_map(move |(method, op)| {
                            Some((path.clone(), method.clone(), op.as_object_mut()?))
                        })
                })
            })
        })
}

/// The object at `path`, creating empty objects along the way. `None` when
/// something on the way is present and is not an object.
fn ensure_object<'a>(
    value: &'a mut Value,
    path: &[&str],
) -> Option<&'a mut serde_json::Map<String, Value>> {
    let mut current = value.as_object_mut()?;
    for key in path {
        current = current
            .entry(*key)
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()?;
    }
    Some(current)
}

pub(crate) fn is_http_method(method: &str) -> bool {
    matches!(method, "get" | "head" | "post" | "put" | "patch" | "delete")
}

/// The server base path a document declares, validated.
///
/// Only a path-only `servers[0].url` counts; a full URL names a host, and the
/// host a request goes to is the profile's to decide, not the spec's.
///
/// `servers[0].url` is prepended to every path, so a hostile one poisons the
/// whole document at once — `"//evil.example"` would turn each path into a
/// scheme-relative URL. Checked here, once, so the failure is reported once and
/// names the real culprit instead of appearing per path.
fn server_prefix(spec: &Value) -> String {
    let declared = spec
        .get("servers")
        .and_then(|s| s.as_array())
        .and_then(|a| a.first())
        .and_then(|s| s.get("url"))
        .and_then(|u| u.as_str())
        .filter(|u| u.starts_with('/'))
        .map(|u| u.trim_end_matches('/'))
        .unwrap_or("");

    if declared.is_empty() {
        return String::new();
    }
    if let Err(e) = crate::endpoint::validate_request_path(declared) {
        // The warning quotes the very string that failed validation, which is
        // the string most likely to be hostile — a rejected path that clears
        // the screen on its way to being reported would hide the report.
        eprintln!(
            "Warning: ignoring API spec server base path '{}': {}",
            crate::sanitize::terminal_text(declared),
            crate::sanitize::terminal_string(e.to_string())
        );
        return String::new();
    }
    declared.to_string()
}

/// `info.version`, the version a document is known by.
fn spec_version_of(spec: &Value) -> Option<String> {
    spec.get("info")?.get("version")?.as_str().map(String::from)
}

/// The version of each document a spec was built from.
///
/// A spec cached before the merge existed carries no map, so its `info.version`
/// stands in as the stable one — otherwise the first check after an upgrade
/// would report a change that never happened.
pub(crate) fn spec_versions(spec: &Value) -> BTreeMap<String, String> {
    if let Some(map) = spec.get(SPEC_VERSIONS_KEY).and_then(|v| v.as_object()) {
        return map
            .iter()
            .filter_map(|(name, version)| Some((name.clone(), version.as_str()?.to_string())))
            .collect();
    }
    spec_version_of(spec)
        .map(|version| BTreeMap::from([("stable".to_string(), version)]))
        .unwrap_or_default()
}

/// How a set of spec versions reads in output: the API's own version, with the
/// beta document's alongside it when there is one.
pub(crate) fn spec_version_display(versions: &BTreeMap<String, String>) -> Option<String> {
    let stable = versions.get("stable")?;
    Some(match versions.get("beta") {
        Some(beta) => format!("{stable} (beta {beta})"),
        None => stable.clone(),
    })
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
pub(crate) fn cache_key(base_url: &str) -> String {
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

/// Every cached spec on disk, across all environments.
///
/// Listed by scanning rather than by asking the config which profiles exist:
/// a spec is cached per base URL, and base URLs arrive from `--base-url` and
/// `ILERT_BASE_URL` too, so the config does not know all of them. Anything a
/// past run left behind is still cache, and a clear that skipped it would be
/// the kind that has to be explained.
///
/// Matched on the exact shape `cache_file_path` writes, so nothing else living
/// in the cache directory — a staged installer, the check markers — is caught
/// by a command that only promised to drop specs.
pub(crate) fn cached_spec_paths() -> Result<Vec<PathBuf>> {
    let cache_dir = ConfigManager::cache_dir()?;
    let Ok(entries) = std::fs::read_dir(&cache_dir) else {
        return Ok(Vec::new());
    };

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n == LEGACY_CACHE_FILE || (n.starts_with("openapi-") && n.ends_with(".json"))
            })
        })
        .collect();
    // Stable order, so the list a clear reports is reproducible.
    paths.sort();
    Ok(paths)
}

/// The version of the spec cached for `base_url`, if there is one — the API's
/// own version, and the beta document's when that environment serves one.
pub(crate) fn cached_spec_version(base_url: &str) -> Option<String> {
    spec_version_display(&spec_versions(&read_cached_spec(base_url)?))
}

/// The cached spec for `base_url`, parsed. A cache that cannot be read or
/// parsed reads as absent; every caller here is reporting state, not relying on
/// it, and `load_index_from_cache` is where an unusable one is dealt with.
pub(crate) fn read_cached_spec(base_url: &str) -> Option<Value> {
    let path = cache_file_path(base_url).ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&content).ok()
}

/// Read the cached spec as a usable index, treating one that cannot be read —
/// or cannot be used — as one that is not there.
///
/// A cache is a copy of something obtainable, so a damaged one is a reason to
/// refetch, never a reason to fail. It used to propagate: a truncated write, a
/// full disk, a hand-edited file — and the error surfaced from `load_cached_index`
/// inside `Cli::new`, before any command exists. That failed *every* invocation
/// with a parse error, including `ilert config cache clear`, the one command
/// whose whole job is to clear it. There was no way out of that state from
/// inside the CLI.
///
/// Indexing happens here, behind the same treatment, rather than in the callers:
/// "parses as JSON" is not the bar, "is an OpenAPI document we can build a
/// command tree from" is. A file holding `{}` clears the parser and then fails
/// in `build_index` for want of `paths` — and a failure there arrives at exactly
/// the same place, before any command exists, with exactly the same lockout.
/// The only definition of a usable cache that helps is the one the caller
/// actually needs.
///
/// The warning is worth the noise: silently refetching a spec that is corrupt on
/// disk would hide a failing disk or a bad write behind a slower startup.
fn load_index_from_cache(path: &PathBuf) -> Result<Option<OperationIndex>> {
    if !path.exists() {
        return Ok(None);
    }

    let unusable = |reason: &str| {
        eprintln!(
            "{} Ignoring the cached API spec at {} ({reason}). It will be refetched; \
             `ilert config cache clear` removes it.",
            "Warning:".yellow().bold(),
            path.display(),
        );
    };

    let Ok(content) = std::fs::read_to_string(path) else {
        unusable("could not be read");
        return Ok(None);
    };
    let Ok(spec) = serde_json::from_str::<Value>(&content) else {
        unusable("is not valid JSON");
        return Ok(None);
    };
    match build_index(&spec) {
        Ok(index) => Ok(Some(index)),
        Err(e) => {
            unusable(&format!("is not a usable OpenAPI document: {e}"));
            Ok(None)
        }
    }
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

    // A merged document states each operation's base path on the operation
    // itself, since the two specs it was built from can declare different ones.
    // `servers[0].url` is the fallback, and it is what a spec cached by an
    // older build of this CLI still carries.
    let document_base = server_prefix(spec);

    // `by_id` is dispatch: `ops run <id>` sends what it finds there, and
    // `OPERATION_OVERRIDES` decides what needs a `--yes` by the same key. So an
    // id is authority, and the beta half of a document does not get to take one
    // that already means something in the stable half.
    let mut claimed_ids = reserved_operation_ids(spec);

    let mut by_id: HashMap<String, Operation> = HashMap::new();
    let mut by_tag: HashMap<String, Vec<Operation>> = HashMap::new();

    for (path, methods) in paths {
        let methods = match methods.as_object() {
            Some(m) => m,
            None => continue,
        };

        for (method, op_value) in methods {
            if !is_http_method(method) {
                continue;
            }

            let base = op_value
                .get(SPEC_BASE_PATH_KEY)
                .and_then(|v| v.as_str())
                .unwrap_or(&document_base);
            let full_path = format!("{base}{path}");
            // Checked here so a path that could re-target a request never
            // becomes a command at all — `ilert alerts list` cannot be made to
            // send the caller's token to `//evil.example` if the operation does
            // not exist. Dropped rather than fatal: one malformed key in a spec
            // we did not write should not take the whole CLI down with it, and
            // `HttpClient::request_raw` refuses the same path again anyway.
            if let Err(e) = validate_spec_path(&full_path) {
                eprintln!(
                    "Warning: ignoring API spec path '{}': {}",
                    crate::sanitize::terminal_text(&full_path),
                    crate::sanitize::terminal_string(e.to_string())
                );
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
            let action = derive_action(method, path, op_value, spec);

            let beta = op_value.get(SPEC_ORIGIN_KEY).and_then(|v| v.as_str()) == Some(BETA_ORIGIN);
            // Settled before the operation is classified, not after: a beta
            // operation that kept a stable id long enough to be classified
            // would inherit that id's `OPERATION_OVERRIDES` entry, and two of
            // those entries mark a `POST` as read-only — which is the
            // difference between a command that asks for confirmation and one
            // that does not.
            let operation_id = match beta {
                true => {
                    claim_beta_operation_id(operation_id(method, path, op_value), &mut claimed_ids)
                }
                false => operation_id(method, path, op_value),
            };

            let parameters = extract_parameters(op_value, spec);
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
                beta,
            };

            by_id.insert(operation_id, op.clone());
            by_tag.entry(tag_normalized).or_default().push(op);
        }
    }

    // Settle the command names within each tag.
    for ops in by_tag.values_mut() {
        // Two keys, in this order.
        //
        // Stable before beta, so the un-suffixed name — `list`, `get` — goes to
        // the one that already had it. Names are claimed in the order walked
        // here, and `paths` iterates in sorted key order, so without this a
        // purely additive beta document could rename an existing command by
        // sorting ahead of it.
        //
        // Then shallowest path first, so the resource itself outranks a lookup
        // hung off it. `/schedules/{id}` and `/schedules/name/{name}` both
        // derive the action `get` — one path ending in a parameter is
        // indistinguishable from another — and sorted key order hands it to
        // `name`, because 'n' < '{'. That renamed the canonical read of twenty
        // resources to `get-<tag>` and gave `get` to a lookup the spec marks
        // NON-PUBLIC, so `ilert schedules get --id 1` became a usage error the
        // moment the spec grew the by-name endpoints. Depth is what separates
        // them: `/schedules/{id}` is the schedule, `/schedules/name/{name}` is
        // one way to find it, and it lands on `get-name`.
        //
        // A stable sort, so operations that tie are still in the spec's order.
        ops.sort_by_key(|op| (op.beta, path_depth(&op.path)));

        let mut claimed: HashSet<String> = HashSet::new();
        for op in ops.iter_mut() {
            op.action = claim_action(&op.action, &op.path, &mut claimed);
        }
    }

    // `by_id` holds copies taken before the names were settled. Bring them up
    // to date, so `ops show <id>` and `ops list` name the same command the
    // command tree does rather than the one it would have been without a
    // collision.
    for ops in by_tag.values() {
        for op in ops {
            if let Some(indexed) = by_id.get_mut(&op.id) {
                indexed.action.clone_from(&op.action);
            }
        }
    }

    Ok(OperationIndex { by_id, by_tag })
}

fn derive_action(method: &str, path: &str, op: &Value, spec: &Value) -> String {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let ends_with_param = segments
        .last()
        .is_some_and(|s| s.starts_with('{') && s.ends_with('}'));

    // A trailing `{param}` usually names *which* resource, so a GET on it is a
    // `get`. Usually — `{filterType}` in `/saved-filters/{filterType}` selects a
    // collection, not a member, and that GET is the list. The path cannot say
    // which kind of parameter it ends with, but the response can: a `get`
    // answers with the resource, a `list` answers with an array of them.
    let reads_a_collection = method == "get" && returns_an_array(op, spec);

    match (method, ends_with_param && !reads_a_collection) {
        ("get", false) => "list".to_string(),
        ("get", true) => "get".to_string(),
        ("post", _) => "create".to_string(),
        ("put", _) => "update".to_string(),
        ("patch", _) => "patch".to_string(),
        ("delete", _) => "delete".to_string(),
        _ => method.to_string(),
    }
}

/// Whether an operation's success response is a JSON array.
///
/// Only the declared `200` body is consulted, and a `$ref` is followed once, so
/// this reads a list response whether the document inlines the array or names
/// it. Anything it cannot see — no schema, a `$ref` that goes nowhere — is not
/// an array, which leaves the path shape to decide as it did before.
fn returns_an_array(op: &Value, spec: &Value) -> bool {
    let Some(schema) = op
        .get("responses")
        .and_then(|responses| responses.get("200"))
        .and_then(|ok| ok.get("content"))
        .and_then(|content| content.get("application/json"))
        .and_then(|json| json.get("schema"))
    else {
        return false;
    };

    let resolved = match schema.get("$ref").and_then(|r| r.as_str()) {
        Some(ref_path) => match follow_ref(ref_path, spec) {
            Some(target) => target,
            None => return false,
        },
        None => schema,
    };

    resolved.get("type").and_then(|t| t.as_str()) == Some("array")
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

/// Every operation id that already means something without the beta document.
///
/// Two sources, and both are authority: the ids the stable half of this
/// document produces, and every id [`classification::OPERATION_OVERRIDES`]
/// names — including one an older stable spec has since dropped, which would
/// otherwise be free for a beta operation to claim along with its
/// classification.
fn reserved_operation_ids(spec: &Value) -> HashSet<String> {
    let mut reserved: HashSet<String> = classification::OPERATION_OVERRIDES
        .iter()
        .map(|(id, _)| (*id).to_string())
        .collect();

    for (path, method, op_value) in operations(spec) {
        if op_value.get(SPEC_ORIGIN_KEY).and_then(|v| v.as_str()) == Some(BETA_ORIGIN) {
            continue;
        }
        reserved.insert(operation_id(method, path, op_value));
    }
    reserved
}

/// The id a beta operation is indexed under: the one it asked for if nothing
/// else has it, and a namespaced one otherwise.
///
/// Renamed rather than dropped — the operation is a command like any other, and
/// it stays reachable under the id `ops list` reports for it. What it does not
/// get is an id whose meaning was established elsewhere.
fn claim_beta_operation_id(id: String, claimed: &mut HashSet<String>) -> String {
    if claimed.insert(id.clone()) {
        return id;
    }
    let namespaced = format!("{BETA_NAMESPACE}{id}");
    if claimed.insert(namespaced.clone()) {
        return namespaced;
    }
    for n in 2.. {
        let numbered = format!("{namespaced}-{n}");
        if claimed.insert(numbered.clone()) {
            return numbered;
        }
    }
    unreachable!("the numbered candidates never run out")
}

/// How many segments deep a path is, parameters counted like any other.
///
/// The tiebreaker for which operation in a tag gets the plain action name. It
/// reads as "closer to the resource wins", which is the whole rule: nothing
/// about an endpoint marks it as the canonical one, but the canonical one is
/// always the shortest path that names the resource.
fn path_depth(path: &str) -> usize {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .count()
}

/// The command name an operation gets inside its tag: its action, or — when
/// something already holds that name — the shortest name built from the tail of
/// its path that nothing holds yet.
///
/// The check is on the *final* name, not on the action it started from. One
/// suffix is not always enough to separate two operations: the stable spec
/// alone puts `.../messages/{id}/reactions` and
/// `.../thread-replies/{id}/reactions` in one tag, and both wanted
/// `create-reactions`. Two commands of the same name is not a cosmetic problem
/// — `clap` refuses to build a command with a duplicated subcommand, so
/// `ilert chat-messages --help` panicked outright. A second document merged
/// into the tree only makes the collisions more likely.
///
/// Names are claimed in the order the caller walks the tag, which is stable
/// operations first — so the shortest name goes to the command that already had
/// it, and a beta endpoint takes a longer one.
///
/// Each candidate is escaped for the same reason [`normalize_tag`] is: it
/// becomes a command name, an index key and a line of `--help`, and all three
/// have to be the same string.
fn claim_action(action: &str, path: &str, claimed: &mut HashSet<String>) -> String {
    let segments: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty() && !s.starts_with('{'))
        .map(|s| crate::sanitize::terminal_string(s.to_lowercase()))
        .collect();

    // `list`, then `list-incidents`, then `list-beta-incidents`: more of the
    // path each time, because a name that says which endpoint it is beats a
    // number that does not.
    let candidates = std::iter::once(action.to_string())
        .chain((1..=segments.len()).map(|take| {
            let tail = segments[segments.len() - take..].join("-");
            format!("{action}-{tail}")
        }))
        // Only reachable if a document describes the same endpoint twice, which
        // no name drawn from the path can separate. Unbounded, so this always
        // terminates with a name.
        .chain((2..).map(|n| format!("{action}-{n}")));

    let name = candidates
        .into_iter()
        .find(|candidate| !claimed.contains(candidate))
        .expect("the numbered candidates never run out");
    claimed.insert(name.clone());
    name
}

fn extract_parameters(op: &Value, spec: &Value) -> Vec<Parameter> {
    let params = match op.get("parameters").and_then(|v| v.as_array()) {
        Some(p) => p,
        None => return Vec::new(),
    };

    params
        .iter()
        .filter_map(|p| {
            // A `$ref`'d parameter is a parameter. This used to return `None`
            // for one — `p.get("name")` on a `{"$ref": ...}` is nothing — which
            // was invisible only for as long as every operation inlined all of
            // its parameters. The beta document declares its opt-in header once
            // and points every operation at it, so a walk that did not follow
            // the pointer would drop a required header from every beta command.
            let p = &dereference(p, spec);
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

/// Resolve a `$ref` wrapper to what it points at, leaving anything else alone.
///
/// Bounded rather than recursive-until-done: a document is free to point one
/// `$ref` at another, and a cycle is a document we still have to survive.
fn dereference<'a>(value: &'a Value, spec: &'a Value) -> &'a Value {
    let mut current = value;
    for _ in 0..MAX_REF_HOPS {
        let Some(target) = current.get("$ref").and_then(|v| v.as_str()) else {
            return current;
        };
        let Some(next) = follow_ref(target, spec) else {
            return current;
        };
        current = next;
    }
    current
}

const MAX_REF_HOPS: usize = 8;

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
    use super::{cache_key, load_index_from_cache};

    /// Every shape of unusable cache has to read as "not there", because the
    /// caller that hits it first runs before any command exists — an error there
    /// takes down `config cache clear` along with everything else, and that is
    /// the one command that could have fixed it.
    #[test]
    fn an_unusable_cached_spec_reads_as_no_cache_at_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, content) in [
            ("truncated.json", "{\"paths\": {"),
            ("garbage.json", "not json at all"),
            // Valid JSON, and still not something a command tree can be built
            // from — the case a parse check alone lets through.
            ("empty-object.json", "{}"),
            ("wrong-document.json", r#"{"kind":"Deployment"}"#),
            ("paths-not-an-object.json", r#"{"paths": []}"#),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, content).expect("write");
            let loaded = load_index_from_cache(&path).expect("an unusable cache is not an error");
            assert!(loaded.is_none(), "{name} should have been ignored");
        }
    }

    #[test]
    fn a_usable_cached_spec_is_indexed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spec.json");
        std::fs::write(
            &path,
            r#"{"paths":{"/alerts":{"get":{"operationId":"getAlerts","tags":["Alerts"]}}}}"#,
        )
        .expect("write");

        let index = load_index_from_cache(&path)
            .expect("a usable cache loads")
            .expect("a usable cache is present");
        assert!(index.by_id.contains_key("getAlerts"));
    }

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

    /// The merge is the one place two documents meet, so what it has to get
    /// right is everything that is only a question when there are two of them:
    /// sibling methods, component names, command names, and each document's own
    /// server prefix.
    mod merging {
        use super::super::{build_index, merge_specs, spec_version_display, spec_versions};
        use serde_json::{Value, json};

        fn stable() -> Value {
            json!({
                "info": {"version": "stable-1"},
                "servers": [{"url": "/api"}],
                "paths": {
                    "/alerts": {"get": {"tags": ["Alerts"], "summary": "List alerts"}},
                    "/incidents": {"get": {"tags": ["Incidents"], "summary": "List incidents"}, "post": {
                        "tags": ["Incidents"],
                        "requestBody": {"content": {"application/json": {
                            "schema": {"$ref": "#/components/schemas/Incident"}
                        }}}
                    }}
                },
                "components": {"schemas": {"Incident": {
                    "type": "object",
                    "properties": {"stableOnly": {"type": "string"}}
                }}}
            })
        }

        fn beta() -> Value {
            json!({
                "info": {"version": "beta-1"},
                "servers": [{"url": "/api"}],
                "paths": {
                    // Sorts before `/incidents`, which is the case that decides
                    // who keeps the `list` command name.
                    "/beta/incidents": {"get": {"tags": ["Incidents"], "summary": "List incidents"}},
                    // A method the stable document does not serve on a path it does.
                    "/alerts": {"post": {
                        "tags": ["Alerts"],
                        "requestBody": {"content": {"application/json": {
                            "schema": {"$ref": "#/components/schemas/Incident"}
                        }}}
                    }}
                },
                "components": {"schemas": {"Incident": {
                    "type": "object",
                    "properties": {"betaOnly": {"type": "string"}}
                }}}
            })
        }

        fn merged() -> Value {
            merge_specs(stable(), Some(beta()))
        }

        fn body_properties(spec: &Value, id: &str) -> Vec<String> {
            let index = build_index(spec).expect("the merged document indexes");
            let op = index.by_id.get(id).unwrap_or_else(|| panic!("no {id}"));
            op.request_body_schema
                .as_ref()
                .and_then(|s| s.get("properties"))
                .and_then(|p| p.as_object())
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default()
        }

        /// A path-level insert would have dropped the stable `GET /alerts` on
        /// the floor the moment beta added a `POST` to the same path.
        #[test]
        fn a_shared_path_keeps_both_documents_methods() {
            let index = build_index(&merged()).expect("indexes");
            assert!(
                index.by_id.contains_key("get-alerts"),
                "the stable GET is gone"
            );
            assert!(
                index.by_id.contains_key("post-alerts"),
                "the beta POST never arrived"
            );
        }

        /// Both documents define `Incident`, and they are not the same shape.
        /// `resolve_refs` follows a pointer against the whole merged document,
        /// so an un-namespaced merge would not lose an argument here — it would
        /// silently re-describe the *stable* command's request body.
        #[test]
        fn a_beta_schema_cannot_rewrite_the_stable_one_of_the_same_name() {
            let merged = merged();
            assert_eq!(body_properties(&merged, "post-incidents"), ["stableOnly"]);
            assert_eq!(body_properties(&merged, "post-alerts"), ["betaOnly"]);
        }

        /// `paths` iterates in sorted key order, so `/beta/incidents` is reached
        /// before `/incidents` — and the un-suffixed name goes to whichever
        /// operation is reached first. A purely additive beta document must not
        /// be able to rename a command that has been shipping.
        #[test]
        fn a_beta_operation_never_takes_a_stable_command_name() {
            let index = build_index(&merged()).expect("indexes");
            let stable_list = index
                .find_by_tag_action("incidents", "list")
                .expect("`incidents list` still exists");
            assert_eq!(stable_list.path, "/api/incidents");
            assert!(!stable_list.beta);

            let beta_list = index
                .find_by_tag_action("incidents", "list-incidents")
                .expect("the beta list is offered under a suffixed name");
            assert_eq!(beta_list.path, "/api/beta/incidents");
            assert!(beta_list.beta);
        }

        /// The stable document is the contract the CLI has been shipping, so it
        /// wins a genuine conflict — same method, same path, two definitions.
        #[test]
        fn the_stable_definition_wins_a_real_conflict() {
            let mut beta = beta();
            beta["paths"]["/alerts"] = json!({
                "get": {"tags": ["Alerts"], "summary": "Beta's own list alerts"}
            });

            let index = build_index(&merge_specs(stable(), Some(beta))).expect("indexes");
            let op = index
                .by_id
                .get("get-alerts")
                .expect("the stable GET survives");
            assert_eq!(op.summary.as_deref(), Some("List alerts"));
            assert!(!op.beta);
        }

        /// One `servers` array cannot describe two documents. Each operation
        /// carries the prefix its own document declared, and the `paths` keys
        /// stay as authored — an operation renamed by the merge would break
        /// every `ops run <id>` and every classification override.
        #[test]
        fn each_document_keeps_its_own_server_prefix() {
            let mut beta = beta();
            beta["servers"] = json!([{"url": "/gateway"}]);

            let index = build_index(&merge_specs(stable(), Some(beta))).expect("indexes");
            assert_eq!(index.by_id["get-alerts"].path, "/api/alerts");
            assert_eq!(
                index.by_id["get-beta-incidents"].path,
                "/gateway/beta/incidents"
            );
        }

        /// The beta document is versioned separately, so a merged spec has to
        /// record both — a beta-only bump that the freshness check could not
        /// see would sit on disk for the whole TTL.
        #[test]
        fn the_merged_document_records_the_version_of_each_document() {
            let versions = spec_versions(&merged());
            assert_eq!(versions["stable"], "stable-1");
            assert_eq!(versions["beta"], "beta-1");
            assert_eq!(
                spec_version_display(&versions).as_deref(),
                Some("stable-1 (beta beta-1)")
            );

            // A beta fetch that failed leaves the key out, so the next check
            // sees a difference and heals rather than caching a spec-minus-beta.
            let versions = spec_versions(&merge_specs(stable(), None));
            assert!(!versions.contains_key("beta"));
            assert_eq!(spec_version_display(&versions).as_deref(), Some("stable-1"));

            // A spec cached before the merge existed carries no map at all; its
            // own version has to stand in, or the first check after an upgrade
            // reports a change that never happened.
            assert_eq!(spec_versions(&stable())["stable"], "stable-1");
        }

        /// A shared `paths` key is not a shared endpoint. Each document brings
        /// its own server prefix, so `/incidents` under `/api` and `/incidents`
        /// under `/api/beta` are two endpoints written the same way — and
        /// reading that as a conflict silently dropped the beta one.
        #[test]
        fn the_same_path_key_under_two_prefixes_is_two_endpoints() {
            let mut beta = beta();
            beta["servers"] = json!([{"url": "/api/beta"}]);
            beta["paths"] = json!({
                "/incidents": {"get": {"tags": ["Incidents"], "summary": "List incidents (beta)"}}
            });

            let index = build_index(&merge_specs(stable(), Some(beta))).expect("indexes");
            let stable_list = index
                .find_by_tag_action("incidents", "list")
                .expect("the stable endpoint is untouched");
            assert_eq!(stable_list.path, "/api/incidents");
            assert!(!stable_list.beta);

            let beta_list = index.by_tag["incidents"]
                .iter()
                .find(|op| op.beta)
                .expect("the beta endpoint is a command, not a dropped conflict");
            assert_eq!(beta_list.path, "/api/beta/incidents");
            assert_eq!(beta_list.method, "GET");
        }

        /// Two commands of one name is not cosmetic: `clap` refuses to build a
        /// command whose subcommands collide, so it takes out `--help` and
        /// everything under that tag. One suffix is not always enough to
        /// separate them — here the stable document already holds both `list`
        /// and `list-incidents` before the beta endpoint asks for a name.
        #[test]
        fn a_beta_endpoint_cannot_land_on_a_name_that_is_taken() {
            let mut stable = stable();
            stable["paths"]["/reports/incidents"] =
                json!({"get": {"tags": ["Incidents"], "summary": "Incident report"}});

            let index = build_index(&merge_specs(stable, Some(beta()))).expect("indexes");
            let named: Vec<(&str, &str)> = index.by_tag["incidents"]
                .iter()
                .map(|op| (op.action.as_str(), op.path.as_str()))
                .collect();

            let unique: std::collections::HashSet<&str> =
                named.iter().map(|(action, _)| *action).collect();
            assert_eq!(
                unique.len(),
                named.len(),
                "duplicate command name in {named:?}"
            );

            // The two that were there keep the names they had...
            assert!(named.contains(&("list", "/api/incidents")));
            assert!(named.contains(&("list-incidents", "/api/reports/incidents")));
            // ...and the newcomer takes one more of its own path.
            assert!(named.contains(&("list-beta-incidents", "/api/beta/incidents")));
        }

        /// The classification table is written against ids, so an id it names
        /// is reserved even when the stable document has since stopped
        /// declaring it — two of those entries mark a `POST` as read-only, and
        /// a beta write that inherited one would preview as a read and skip the
        /// confirmation a write gets.
        #[test]
        fn a_beta_operation_cannot_take_an_id_the_classification_table_names() {
            let mut beta = beta();
            beta["paths"]["/beta/impostor"] = json!({"post": {
                "operationId": "post-incidents-publish-info",
                "tags": ["Alerts"]
            }});

            let index = build_index(&merge_specs(stable(), Some(beta))).expect("indexes");
            assert!(
                !index.by_id.contains_key("post-incidents-publish-info"),
                "the beta operation took a reserved id"
            );

            let op = &index.by_id["beta.post-incidents-publish-info"];
            assert!(op.beta);
            assert!(
                !op.classification.read_only,
                "a write inherited the read-only classification of the id it asked for"
            );
        }

        /// The beta document declares its opt-in header once and points every
        /// operation at it. A parameter walk that did not follow the pointer
        /// dropped a *required* header from every beta command — and the
        /// failure would have arrived from the server, as a 4xx.
        #[test]
        fn a_referenced_parameter_reaches_the_operation() {
            let mut beta = beta();
            beta["components"]["parameters"] = json!({"IlertBetaHeader": {
                "name": "ilert-beta",
                "in": "header",
                "required": true,
                "schema": {"type": "string", "default": "v1"}
            }});
            beta["paths"]["/beta/incidents"]["get"]["parameters"] =
                json!([{"$ref": "#/components/parameters/IlertBetaHeader"}]);

            let index = build_index(&merge_specs(stable(), Some(beta))).expect("indexes");
            let op = &index.by_id["get-beta-incidents"];
            let header = op
                .parameters
                .iter()
                .find(|p| p.name == "ilert-beta")
                .expect("the referenced header is a parameter like any other");
            assert!(header.required);
            // ...and one the CLI fills in, so a beta command is no more typing
            // than a stable one.
            assert_eq!(header.default_value().as_deref(), Some("v1"));
        }
    }

    /// Command names are settled inside a tag, and two operations can want the
    /// same one without a beta document being involved at all.
    mod naming {
        use super::super::build_index;
        use serde_json::json;

        /// The shape the stable spec has shipped for as long as it has had
        /// thread replies: `.../messages/{id}/reactions` and
        /// `.../thread-replies/{id}/reactions` share their last segment, so one
        /// suffix left both of them called `create-reactions` — and
        /// `ilert chat-messages --help` panicked inside `clap` rather than
        /// printing anything.
        #[test]
        fn operations_that_share_a_path_tail_still_get_distinct_names() {
            let spec = json!({
                "paths": {
                    // The plain create takes the bare name, which is what left
                    // the other two contending for the same suffixed one.
                    "/messages": {
                        "post": {"tags": ["Chat Messages"], "summary": "Post a message"}
                    },
                    "/messages/{id}/reactions": {
                        "post": {"tags": ["Chat Messages"], "summary": "React to a message"}
                    },
                    "/messages/{threadId}/thread-replies/{id}/reactions": {
                        "post": {"tags": ["Chat Messages"], "summary": "React to a reply"}
                    }
                }
            });

            let index = build_index(&spec).expect("indexes");
            let actions: Vec<&str> = index.by_tag["chat-messages"]
                .iter()
                .map(|op| op.action.as_str())
                .collect();
            let unique: std::collections::HashSet<&&str> = actions.iter().collect();
            assert_eq!(unique.len(), actions.len(), "duplicate name in {actions:?}");
            assert!(
                actions.contains(&"create")
                    && actions.contains(&"create-reactions")
                    && actions.contains(&"create-thread-replies-reactions"),
                "got {actions:?}"
            );
        }

        /// `ops show <id>` reads the operation out of `by_id`, which held a copy
        /// taken before the names were settled — so it answered `list` for a
        /// command the tree calls `list-incidents`.
        #[test]
        fn the_id_index_reports_the_name_the_command_tree_uses() {
            let spec = json!({
                "paths": {
                    "/incidents": {"get": {"tags": ["Incidents"], "summary": "List"}},
                    "/reports/incidents": {"get": {"tags": ["Incidents"], "summary": "Report"}}
                }
            });

            let index = build_index(&spec).expect("indexes");
            assert_eq!(index.by_id["get-incidents"].action, "list");
            assert_eq!(
                index.by_id["get-reports-incidents"].action,
                "list-incidents"
            );
        }

        /// The reported bug. A path ending in a parameter is a `get` whichever
        /// parameter it is, so `/schedules/{id}` and `/schedules/name/{name}`
        /// both wanted `get` — and sorted path order gave it to `name`, because
        /// 'n' sorts before '{'. Twenty resources had their canonical read
        /// renamed to `get-<tag>` the day the spec grew its by-name lookups, so
        /// `ilert schedules get --id 1` answered "unexpected argument '--id'".
        #[test]
        fn the_resource_path_outranks_a_lookup_hung_off_it() {
            let spec = json!({
                "paths": {
                    "/schedules": {"get": {"tags": ["Schedules"], "summary": "List"}},
                    "/schedules/name/{name}": {
                        "get": {"tags": ["Schedules"], "summary": "By name"}
                    },
                    "/schedules/{id}": {"get": {"tags": ["Schedules"], "summary": "By id"}}
                }
            });

            let index = build_index(&spec).expect("indexes");
            assert_eq!(index.by_id["get-schedules-id"].action, "get");
            assert_eq!(index.by_id["get-schedules-name-name"].action, "get-name");
            assert_eq!(index.by_id["get-schedules"].action, "list");
        }

        /// `{filterType}` selects a collection, not a member, so
        /// `/saved-filters/{filterType}` is the list — but the path cannot say
        /// that, and the trailing parameter made it a `get`. It then outranked
        /// `/{filterType}/{id}` on depth, so the read of a single filter was
        /// called `get-saved-filters` and the list was called `get`. The
        /// response shape is what tells them apart.
        #[test]
        fn a_get_that_answers_with_an_array_is_a_list() {
            let object = json!({"200": {"content": {"application/json": {
                "schema": {"$ref": "#/components/schemas/Filter"}
            }}}});
            let array = json!({"200": {"content": {"application/json": {
                "schema": {"type": "array", "items": {"$ref": "#/components/schemas/Filter"}}
            }}}});
            let spec = json!({
                "paths": {
                    "/saved-filters/{filterType}": {
                        "get": {"tags": ["Saved Filters"], "responses": array}
                    },
                    "/saved-filters/{filterType}/{id}": {
                        "get": {"tags": ["Saved Filters"], "responses": object}
                    }
                },
                "components": {"schemas": {"Filter": {"type": "object"}}}
            });

            let index = build_index(&spec).expect("indexes");
            assert_eq!(index.by_id["get-saved-filters-filterType"].action, "list");
            assert_eq!(index.by_id["get-saved-filters-filterType-id"].action, "get");
        }

        /// The array may be named rather than inlined, so the `$ref` is followed
        /// once. A document that describes its list response as a component is
        /// making the same statement as one that spells it out.
        #[test]
        fn a_named_array_response_reads_as_a_list_too() {
            let spec = json!({
                "paths": {"/filters/{filterType}": {"get": {
                    "tags": ["Filters"],
                    "responses": {"200": {"content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/FilterList"}
                    }}}}
                }}},
                "components": {"schemas": {
                    "FilterList": {"type": "array", "items": {"type": "object"}}
                }}
            });

            let index = build_index(&spec).expect("indexes");
            assert_eq!(index.by_id["get-filters-filterType"].action, "list");
        }

        /// The rule reads the response, not the method: an operation the
        /// document says nothing about stays whatever its path shape made it,
        /// so a spec without response schemas names its commands as before.
        #[test]
        fn an_undescribed_response_leaves_the_path_shape_deciding() {
            let spec = json!({
                "paths": {
                    "/teams": {"get": {"tags": ["Teams"], "summary": "List"}},
                    "/teams/{id}": {"get": {"tags": ["Teams"], "summary": "Read"}}
                }
            });

            let index = build_index(&spec).expect("indexes");
            assert_eq!(index.by_id["get-teams"].action, "list");
            assert_eq!(index.by_id["get-teams-id"].action, "get");
        }

        /// Depth is the *second* key. Both of these derive `get`, and the beta
        /// one is the shallower — but a beta document must never rename a
        /// stable command, so the stable operation still takes the bare name.
        #[test]
        fn a_shorter_beta_path_still_does_not_outrank_a_stable_command() {
            let stable = json!({
                "paths": {
                    "/teams/{id}/members/{memberId}": {
                        "get": {"tags": ["Teams"], "summary": "Stable read"}
                    }
                }
            });
            let beta = json!({
                "paths": {"/teams/{id}": {"get": {"tags": ["Teams"], "summary": "Beta read"}}}
            });

            let index =
                build_index(&super::super::merge_specs(stable, Some(beta))).expect("indexes");
            let named = |action: &str| {
                index.by_tag["teams"]
                    .iter()
                    .find(|op| op.action == action)
                    .unwrap_or_else(|| {
                        panic!(
                            "nothing is called `{action}` in {:?}",
                            index.by_tag["teams"]
                                .iter()
                                .map(|op| &op.action)
                                .collect::<Vec<_>>()
                        )
                    })
            };
            assert_eq!(named("get").path, "/teams/{id}/members/{memberId}");
            assert!(
                named("get-teams").beta,
                "the beta operation is the other one"
            );
        }
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
                beta: false,
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

        #[test]
        fn a_list_parameter_is_the_one_the_schema_calls_an_array() {
            assert!(query_param("include", Some(serde_json::json!({"type": "array"}))).is_list());
            assert!(!query_param("q", Some(serde_json::json!({"type": "string"}))).is_list());
            // A parameter the spec left untyped is a single value, not a list:
            // repeating it would be a guess about a shape nobody declared.
            assert!(!query_param("q", None).is_list());
        }
    }
}
