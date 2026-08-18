use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use colored::Colorize;
use serde_json::Value;

use crate::config::ConfigManager;
use crate::endpoint::Endpoint;
use crate::sanitize::terminal_text;

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
        // `info.version` is free-form text out of the OpenAPI document, so it is
        // the server's string, not ours — and here it is the whole line.
        println!("api   {}", terminal_text(&api_ver));
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

/// `latest` is `tag_name` from the GitHub releases API. `is_newer_semver` only
/// compares the dot-separated numeric prefix, so a tag like `9.9.9<ESC>]52;…`
/// clears that gate and everything after the digits still reaches the terminal.
/// A different host from the ilert API, but an HTTP response all the same.
fn print_cli_update_notice(latest: &str) {
    eprintln!("{}", cli_update_notice(latest));
}

/// The notice names the command that acts on it. A version number on its own
/// leaves the reader to work out how this binary was installed and what the
/// matching upgrade is; `ilert update` answers that for every install path the
/// installer owns, and tells the rest which manager owns them.
fn cli_update_notice(latest: &str) -> String {
    format!(
        "{} A new version of ilert is available: {} -> {} — run {}",
        "Update:".cyan().bold(),
        current_version().dimmed(),
        terminal_text(latest).green().bold(),
        "ilert update".bold(),
    )
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
///
/// This runs on a detached background task, so it does not get to invent its own
/// rules: the URL is resolved through [`Endpoint`] rather than concatenated, and
/// the document comes back through [`crate::openapi::fetch_spec`], which is the
/// one fetcher with redirects disabled, a timeout, a status check and a size
/// cap. It also has to read and write the same per-environment cache file
/// `openapi.rs` owns — the old fixed `openapi.json` name is the pre-per-base-url
/// path that `save_to_cache` now deletes, so this check was reading a file that
/// no longer exists and would have written its refresh where nothing looks.
async fn try_check_api() -> Result<()> {
    let config_manager = ConfigManager::load()?;
    let resolved = config_manager.resolve(None, None, None, None, None);

    let spec_path = crate::openapi::cache_file_path(&resolved.base_url)?;
    if !spec_path.exists() {
        return Ok(());
    }

    let cached_version = read_spec_version(&spec_path).unwrap_or_default();
    if cached_version.is_empty() {
        return Ok(());
    }

    let spec_url = Endpoint::parse(&resolved.base_url)?.resolve("/api-docs/openapi.json")?;
    let fresh_spec = crate::openapi::fetch_spec(&spec_url).await?;
    let fresh_version = fresh_spec
        .get("info")
        .and_then(|i| i.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !fresh_version.is_empty() && fresh_version != cached_version {
        crate::openapi::save_to_cache(&spec_path, &fresh_spec)?;

        eprintln!("{}", api_update_notice(&cached_version, fresh_version));
    }

    Ok(())
}

/// Both versions are `info.version` out of an OpenAPI document — one fetched
/// just now, one read back from a previous fetch. This notice can interleave
/// with any command's output, since it comes from a detached task spawned in
/// `cli.rs`.
fn api_update_notice(cached: &str, fresh: &str) -> String {
    format!(
        "{} API spec updated: {} -> {}",
        "Note:".cyan().bold(),
        terminal_text(cached).dimmed(),
        terminal_text(fresh).green(),
    )
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

/// Where the twelve-hour "latest vs current" result is cached. `update` deletes
/// it after a successful install, since both halves of that comparison change.
pub(crate) fn check_file_path() -> Result<PathBuf> {
    let cache_dir = ConfigManager::cache_dir()?;
    Ok(cache_dir.join(CHECK_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The version gate only compares the dot-separated numeric prefix, so
    /// everything a tag carries after the digits rides along into the notice.
    #[test]
    fn a_hostile_release_tag_clears_the_version_gate() {
        let tag = "999.0.0\u{1b}]52;c;cm0gLXJmIC8=\u{7}";
        assert!(
            is_newer_semver(tag, current_version()),
            "the gate is what makes escaping this necessary"
        );
    }

    #[test]
    fn an_update_notice_cannot_carry_an_escape_sequence() {
        let _colors = crate::testutil::colors(false);

        for hostile in [
            "999.0.0\u{1b}]52;c;cm0gLXJmIC8=\u{7}",
            "999.0.0\u{1b}[2J",
            "999.0.0\nUpdate: nothing to do",
            "999.0.0\u{9b}31m",
            "999.0.0\u{202E}",
        ] {
            let cli = cli_update_notice(hostile);
            let api = api_update_notice(hostile, hostile);
            for notice in [&cli, &api] {
                assert!(!notice.contains('\u{1b}'), "{hostile:?} -> {notice:?}");
                assert!(!notice.contains('\u{7}'), "{hostile:?} -> {notice:?}");
                assert!(!notice.contains('\n'), "{hostile:?} -> {notice:?}");
                assert!(!notice.contains('\u{9b}'), "{hostile:?} -> {notice:?}");
                assert!(!notice.contains('\u{202E}'), "{hostile:?} -> {notice:?}");
            }
        }
    }

    #[test]
    fn an_ordinary_notice_still_reads_normally() {
        let _colors = crate::testutil::colors(false);
        assert_eq!(
            cli_update_notice("2.1.0"),
            format!(
                "Update: A new version of ilert is available: {} -> 2.1.0 — run ilert update",
                current_version()
            )
        );
        assert_eq!(
            api_update_notice("1.0.0", "1.1.0"),
            "Note: API spec updated: 1.0.0 -> 1.1.0"
        );
    }
}
