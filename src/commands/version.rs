use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use colored::Colorize;
use serde_json::Value;

use crate::config::ConfigManager;

const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60); // 12 hours
const GITHUB_REPO: &str = "iLert/ilert-cli";
const CHECK_FILE: &str = "update-check.json";

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Version info: the two aligned lines are the default rendering, and a caller
/// that asked for data — `-o`, `--fields`, `--jq` — gets it through the shared
/// output path instead.
///
/// The condition is `format_requested` rather than the resolved format on
/// purpose. `ilert version` piped into something is a long-standing way to read
/// the version out of a script, and the format falls back to JSON off a
/// terminal, so keying on the format alone would rewrite that output for
/// everyone who never asked.
pub fn handle(ctx: &crate::cli::RunContext) -> Result<()> {
    if !ctx.format_requested && ctx.jq.is_none() && ctx.fields.is_none() {
        print_version();
        return Ok(());
    }

    ctx.print(&serde_json::json!({
        "cli": current_version(),
        "api": cached_api_version(),
    }))
}

/// Print version info including cached API version.
pub fn print_version() {
    println!("ilert {}", current_version());
    if let Some(api_ver) = cached_api_version() {
        println!("api   {api_ver}");
    }
}

/// Check for CLI updates and API spec freshness in the background.
pub async fn check_for_updates() {
    let _ = try_check_cli().await;
    let _ = try_check_api().await;
}

// ---------------------------------------------------------------------------
// CLI version check (GitHub releases)
// ---------------------------------------------------------------------------

async fn try_check_cli() -> Result<()> {
    let cache_path = check_file_path()?;

    if let Ok(metadata) = std::fs::metadata(&cache_path)
        && let Ok(modified) = metadata.modified()
    {
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::MAX);
        if age < CHECK_INTERVAL {
            show_cached_result(&cache_path)?;
            return Ok(());
        }
    }

    let client = crate::client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let mut req = client.get(&url);

    if let Ok(token) = std::env::var("GH_TOKEN").or_else(|_| std::env::var("GITHUB_TOKEN")) {
        req = req.header("Authorization", format!("Bearer {token}"));
    }

    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Ok(());
    }

    let release: Value = resp.json().await?;
    let latest_tag = release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let latest_version = latest_tag.trim_start_matches('v');

    let result = serde_json::json!({
        "latest": latest_version,
        "current": current_version(),
    });
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&cache_path, serde_json::to_string(&result)?);

    if is_newer_semver(latest_version, current_version()) {
        print_cli_update_notice(latest_version);
    }

    Ok(())
}

fn show_cached_result(path: &PathBuf) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let cached: Value = serde_json::from_str(&content)?;
    if let Some(latest) = cached.get("latest").and_then(|v| v.as_str())
        && is_newer_semver(latest, current_version())
    {
        print_cli_update_notice(latest);
    }
    Ok(())
}

fn print_cli_update_notice(latest: &str) {
    eprintln!(
        "{} A new version of ilert is available: {} -> {}",
        "Update:".cyan().bold(),
        current_version().dimmed(),
        latest.green().bold(),
    );
}

fn is_newer_semver(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> { s.split('.').filter_map(|p| p.parse().ok()).collect() };
    parse(latest) > parse(current)
}

// ---------------------------------------------------------------------------
// API version check (OpenAPI spec info.version)
// ---------------------------------------------------------------------------

/// Check if the remote API has a newer version than the cached spec.
/// If so, refresh the spec cache silently.
async fn try_check_api() -> Result<()> {
    let cache_dir = ConfigManager::cache_dir()?;
    let spec_path = cache_dir.join("openapi.json");

    if !spec_path.exists() {
        return Ok(());
    }

    let cached_version = read_spec_version(&spec_path).unwrap_or_default();
    if cached_version.is_empty() {
        return Ok(());
    }

    // Resolve base_url from config
    let config_manager = ConfigManager::load()?;
    let resolved = config_manager.resolve(None, None, None, None);
    let spec_url = format!(
        "{}/api-docs/openapi.json",
        resolved.base_url.trim_end_matches('/')
    );

    // Lightweight HEAD-like check: fetch the spec and compare info.version
    let client = crate::client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let resp = client.get(&spec_url).send().await?;
    if !resp.status().is_success() {
        return Ok(());
    }

    let fresh_spec: Value = resp.json().await?;
    let fresh_version = fresh_spec
        .get("info")
        .and_then(|i| i.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !fresh_version.is_empty() && fresh_version != cached_version {
        // API has been updated — refresh cache
        let content = serde_json::to_string(&fresh_spec)?;
        let _ = std::fs::write(&spec_path, content);

        eprintln!(
            "{} API spec updated: {} -> {}",
            "Note:".cyan().bold(),
            cached_version.dimmed(),
            fresh_version.green(),
        );
    }

    Ok(())
}

/// Read info.version from a cached OpenAPI spec file.
fn read_spec_version(path: &PathBuf) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let spec: Value = serde_json::from_str(&content).ok()?;
    spec.get("info")
        .and_then(|i| i.get("version"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Get the API version from the cached spec (for display).
fn cached_api_version() -> Option<String> {
    let cache_dir = ConfigManager::cache_dir().ok()?;
    let spec_path = cache_dir.join("openapi.json");
    read_spec_version(&spec_path)
}

fn check_file_path() -> Result<PathBuf> {
    let cache_dir = ConfigManager::cache_dir()?;
    Ok(cache_dir.join(CHECK_FILE))
}
