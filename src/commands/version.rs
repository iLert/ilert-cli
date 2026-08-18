use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use colored::Colorize;
use serde_json::Value;

use crate::config::ConfigManager;
use crate::endpoint::Endpoint;
use crate::sanitize::terminal_text;

// Cache-clock architecture
// ------------------------
// The OpenAPI cache deliberately has two clocks with different meanings:
//
// * `openapi-<environment>.json` mtime is the last successful spec write. It
//   drives `openapi::ensure_spec`'s 24-hour in-band freshness decision.
// * `api-check-<environment>.json` mtime is the last background check attempt.
//   It throttles automatic checks to `CHECK_INTERVAL`, including failed or
//   externally timed-out attempts.
//
// Do not collapse these into the spec mtime. Touching the spec after a failed
// background request would make `ensure_spec` trust old data for another full
// TTL even though no new spec was obtained. The GitHub release result has its
// own machine-wide clock in `update-check.json`; unlike specs, it is not tied
// to an ilert environment.

/// How long a "latest vs current" result stands before it is fetched again.
///
/// An hour rather than a day: a release should be visible to someone running
/// the CLI the same afternoon it ships, and one unconditional request per hour
/// per machine is nothing next to GitHub's unauthenticated rate limit.
const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
const GITHUB_REPO: &str = "iLert/ilert-cli";
const CHECK_FILE: &str = "update-check.json";

/// Prefix for the per-environment record of when the spec check last ran.
///
/// Its own file rather than the spec's mtime, because that mtime already means
/// something else: `openapi.rs` reads it as "last known good content" for the
/// 24-hour TTL behind `ensure_spec`. Touching it here to note an attempt would
/// extend that TTL on the strength of a request that may have failed — a check
/// that learned nothing would suppress the refresh that actually matters.
///
/// One file per environment, keyed the same way the spec cache is. A single
/// shared marker made switching environments pathological: each invocation
/// found the marker recording the *other* base URL, treated it as not
/// applicable, and downloaded the whole spec again — so alternating between two
/// profiles meant a full download on every command, which is precisely what the
/// interval exists to prevent.
const API_CHECK_PREFIX: &str = "api-check-";

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
pub fn handle(ctx: &crate::cli::RunContext, base_url: &str) -> Result<()> {
    if !ctx.format_requested && ctx.jq.is_none() && ctx.fields.is_none() {
        println!("{}", version_block(base_url));
        return Ok(());
    }

    ctx.print(&serde_json::json!({
        "cli": current_version(),
        "api": cached_api_version(base_url),
    }))
}

/// The human-readable version block, shared by `ilert version` and `--version`.
///
/// One renderer for both because they are the same question asked two ways, and
/// a reader who tries the other spelling after seeing one should not get a
/// different answer. clap owns the `--version` path and takes a plain string,
/// which is the whole reason this returns one instead of printing.
///
/// The spec line is coloured and the CLI line is not, on the grounds that the
/// binary's own version is what `--version` is conventionally scraped for and
/// should stay as close to bare text as possible; the spec version is the extra
/// this adds, so it is the part that gets marked as extra. Colour is dropped
/// automatically off a terminal and under `NO_COLOR`, so a script reading either
/// line still sees plain text.
pub fn version_block(base_url: &str) -> String {
    prefixed_version_block(&format!("ilert {}", current_version()), base_url)
}

/// The same block for clap's `--version`, which prints the binary name itself.
///
/// Only the lead line differs: clap renders `{name} {version}`, so handing it
/// the full block from [`version_block`] produces "ilert ilert 0.3.0".
pub fn clap_version_block(base_url: &str) -> String {
    prefixed_version_block(current_version(), base_url)
}

fn prefixed_version_block(lead: &str, base_url: &str) -> String {
    let mut block = lead.to_string();
    if let Some(api_ver) = cached_api_version(base_url) {
        // `info.version` is free-form text out of the OpenAPI document, so it is
        // the server's string, not ours — and here it is the whole line.
        block.push_str(&format!(
            "\n{}   {}",
            "api".dimmed(),
            terminal_text(&api_ver).cyan()
        ));
    }
    block
}

/// Print the CLI update notice for a result we already have.
///
/// Cache-only and synchronous, and called before the command runs. Both halves
/// of that are deliberate. Reading a small file costs nothing, so this cannot
/// delay the command the way awaiting a request would; and printing here puts
/// the notice above the command's own output instead of somewhere in the middle
/// of it, which is where a task that prints whenever its response happens to
/// arrive would land it.
///
/// Nothing is printed when the cached result has aged out — [`refresh_checks`]
/// is fetching a new one in that case and prints it itself, so the two are
/// mutually exclusive on the same freshness test and cannot both fire.
pub fn print_cached_update_notice() {
    let Ok(path) = check_file_path() else {
        return;
    };
    if !cache_is_fresh(&path) {
        return;
    }
    let _ = show_cached_result(&path);
}

/// Refresh both cached checks, concurrently.
///
/// Spawned alongside the command and awaited under a short timeout once it
/// finishes — see the call site in `cli.rs`. It must be awaited: a detached
/// task is cancelled when the runtime is dropped at the end of `main`, and an
/// ordinary command returns in tens of milliseconds, so the request never
/// survived long enough to write the cache, let alone print anything.
pub async fn refresh_checks(base_url: &str) {
    let (cli, api) = tokio::join!(try_check_cli(), try_check_api(base_url));
    let _ = cli;
    let _ = api;
}

/// Whether the cached result is young enough to stand without a new request.
///
/// A file we cannot stat or whose mtime the clock disagrees with counts as
/// stale, so an unreadable cache causes a refetch rather than a silence.
fn cache_is_fresh(path: &Path) -> bool {
    let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age < CHECK_INTERVAL)
}

// ---------------------------------------------------------------------------
// CLI version check (GitHub releases)
// ---------------------------------------------------------------------------

async fn try_check_cli() -> Result<()> {
    let cache_path = check_file_path()?;

    // The cached result has already been printed by `print_cached_update_notice`
    // if it says anything, so there is nothing left to do but leave it alone.
    if cache_is_fresh(&cache_path) {
        return Ok(());
    }

    // The attempt is recorded *before* it is made, not after. Recording it
    // afterwards only covers a request that returns — but this task is awaited
    // under a budget in `cli.rs` that is deliberately shorter than the request
    // timeouts beneath it, so a network that accepts connections and then
    // stalls has the task dropped before any line after the `await` can run.
    // Nothing would be written, the interval would never start, and every
    // subsequent command would pay the full budget again for the same doomed
    // request. Claiming the interval up front costs at most one lost result,
    // and a lost result is what a failure looks like anyway.
    record_attempt(&cache_path);

    let outcome = fetch_latest_version().await;
    let Ok(latest_version) = outcome else {
        // The notice this run would have printed up front had the result not
        // just aged out. Nothing replaced that result, so it is still the best
        // answer there is, and staying silent about a known update because a
        // refresh failed is the one outcome worth avoiding here. It cannot
        // double up: `print_cached_update_notice` prints only what is fresh,
        // and reaching this line means it was not.
        let _ = show_cached_result(&cache_path);
        return outcome.map(|_| ());
    };

    let result = serde_json::json!({
        "latest": latest_version,
        "current": current_version(),
    });
    write_check_file(&cache_path, &result);

    if is_newer_semver(&latest_version, current_version()) {
        print_cli_update_notice(&latest_version);
    }

    Ok(())
}

/// Ask GitHub for the tag of the latest release.
async fn fetch_latest_version() -> Result<String> {
    let client = crate::client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let mut req = client.get(&url);

    if let Ok(token) = std::env::var("GH_TOKEN").or_else(|_| std::env::var("GITHUB_TOKEN")) {
        req = req.header("Authorization", format!("Bearer {token}"));
    }

    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("the releases API answered {status}");
    }

    let release: Value = resp.json().await?;
    let latest_tag = release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    Ok(latest_tag.trim_start_matches('v').to_string())
}

/// Start the interval before making the request it belongs to.
///
/// An existing result keeps its contents and only has its clock reset: the last
/// answer we did get is still the best one available, and a notice it earned
/// should go on being shown rather than disappearing because a later request
/// failed. With no previous result there is nothing to preserve, and a record
/// with no `latest` is what "checked, learned nothing" looks like —
/// `show_cached_result` reads it as nothing to say rather than as up to date.
fn record_attempt(cache_path: &Path) {
    if let Ok(previous) = std::fs::read_to_string(cache_path) {
        let _ = std::fs::write(cache_path, previous);
        return;
    }
    write_check_file(
        cache_path,
        &serde_json::json!({ "current": current_version() }),
    );
}

fn write_check_file(cache_path: &Path, result: &Value) {
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(result) {
        let _ = std::fs::write(cache_path, text);
    }
}

fn show_cached_result(path: &Path) -> Result<()> {
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
async fn try_check_api(base_url: &str) -> Result<()> {
    let spec_path = crate::openapi::cache_file_path(base_url)?;
    if !spec_path.exists() {
        return Ok(());
    }

    // On the same interval as the release check, and for a sharper reason: this
    // downloads the whole spec every time it runs. Ungated it did so on every
    // command — which cost nothing only for as long as the task was being
    // cancelled before its request finished. Now that the refresh is awaited,
    // an ungated check would put a full spec download in front of every
    // command's exit.
    // Two ways to already be current, and both have to count. The marker
    // records a check that found nothing to change — which leaves the spec file
    // untouched, so its own mtime cannot carry that. The spec's mtime records
    // the opposite case: it was written by a fetch, and a document downloaded
    // inside the interval needs no second opinion about whether it is stale.
    // Without that second test every cold start downloads the spec twice —
    // once through `ensure_spec`, once more here because no marker exists yet.
    let marker = api_check_file_path(base_url)?;
    if cache_is_fresh(&marker) || cache_is_fresh(&spec_path) {
        return Ok(());
    }

    let cached_version = read_spec_version(&spec_path).unwrap_or_default();
    if cached_version.is_empty() {
        return Ok(());
    }

    // Claimed before the request, for the reason spelled out in `try_check_cli`:
    // the outer budget can drop this task mid-flight, and an interval recorded
    // only on the way back would never start.
    write_api_check_marker(&marker, base_url, &cached_version);

    let spec_url = Endpoint::parse(base_url)?.resolve("/api-docs/openapi.json")?;
    let outcome = crate::openapi::fetch_spec(&spec_url).await;
    let Ok(fresh_spec) = outcome else {
        return outcome.map(|_| ());
    };
    let fresh_version = fresh_spec
        .get("info")
        .and_then(|i| i.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    write_api_check_marker(&marker, base_url, fresh_version);

    if !fresh_version.is_empty() && fresh_version != cached_version {
        crate::openapi::save_to_cache(&spec_path, &fresh_spec)?;

        eprintln!("{}", api_update_notice(&cached_version, fresh_version));
    }

    Ok(())
}

fn write_api_check_marker(marker: &Path, base_url: &str, spec_version: &str) {
    write_check_file(
        marker,
        &serde_json::json!({ "base_url": base_url, "spec_version": spec_version }),
    );
}

/// The latest release recorded by the last successful check, if there was one.
///
/// `None` covers both "never checked" and "checked and learned nothing", which
/// a caller reporting cache state wants to render the same way: as no answer,
/// not as up to date.
pub(crate) fn cached_latest_version() -> Option<String> {
    let path = check_file_path().ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&content)
        .ok()?
        .get("latest")?
        .as_str()
        .map(String::from)
}

/// Where the spec check for `base_url` records its last run.
pub(crate) fn api_check_file_path(base_url: &str) -> Result<PathBuf> {
    let name = format!(
        "{API_CHECK_PREFIX}{}.json",
        crate::openapi::cache_key(base_url)
    );
    Ok(ConfigManager::cache_dir()?.join(name))
}

/// Every spec-check marker on disk, across all environments.
///
/// Enumerated rather than derived from the config, for the same reason the spec
/// caches are: base URLs also arrive from `--base-url` and `ILERT_BASE_URL`, so
/// the config does not know all of the ones a past run may have written.
pub(crate) fn api_check_paths() -> Result<Vec<PathBuf>> {
    let cache_dir = ConfigManager::cache_dir()?;
    let Ok(entries) = std::fs::read_dir(&cache_dir) else {
        return Ok(Vec::new());
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(API_CHECK_PREFIX) && n.ends_with(".json"))
        })
        .collect();
    paths.sort();
    Ok(paths)
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

/// `info.version` of the spec cached for this environment, for display.
///
/// Per environment, like every other reader of the spec cache. This used to
/// look at the flat `openapi.json`, which is the pre-per-environment name that
/// `save_to_cache` deletes on sight — so it was reading a file that no longer
/// exists, and the API line simply never appeared.
fn cached_api_version(base_url: &str) -> Option<String> {
    crate::openapi::cached_spec_version(base_url)
}

/// Where the "latest vs current" result is cached for [`CHECK_INTERVAL`].
/// `update` deletes
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
    /// The freshness test decides which of the two printers speaks, so getting
    /// it wrong either silences the notice or prints it twice.
    #[test]
    fn a_result_within_the_interval_is_fresh_and_an_older_one_is_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CHECK_FILE);
        std::fs::write(&path, r#"{"latest":"9.9.9","current":"0.0.1"}"#).expect("write");

        assert!(cache_is_fresh(&path), "a result just written is fresh");

        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("reopen");
        file.set_modified(SystemTime::now() - CHECK_INTERVAL - Duration::from_secs(60))
            .expect("backdate");

        assert!(
            !cache_is_fresh(&path),
            "a result older than the interval has to be refetched"
        );
    }

    /// A cache that is not there at all is the first-run case, and it has to
    /// read as stale — treating it as fresh would mean never fetching one.
    #[test]
    fn a_missing_result_is_not_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!cache_is_fresh(&dir.path().join(CHECK_FILE)));
    }

    /// A failed check has to leave the last good answer intact — a notice that
    /// was earned should not vanish because GitHub was briefly unreachable.
    #[test]
    fn recording_an_attempt_keeps_the_last_answer_and_resets_its_clock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CHECK_FILE);
        let answer = r#"{"latest":"9.9.9","current":"0.0.1"}"#;
        std::fs::write(&path, answer).expect("write");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("reopen")
            .set_modified(SystemTime::now() - CHECK_INTERVAL - Duration::from_secs(60))
            .expect("backdate");

        record_attempt(&path);

        assert_eq!(std::fs::read_to_string(&path).expect("read"), answer);
        assert!(
            cache_is_fresh(&path),
            "the attempt has to count, or every command retries a request that just failed"
        );
    }

    /// The first-run failure has nothing to preserve, and what it writes must
    /// not read as "you are up to date" — that would suppress the real notice
    /// for an hour on the strength of a request that never got an answer.
    #[test]
    fn a_first_attempt_records_no_version_at_all() {
        let _colors = crate::testutil::colors(false);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CHECK_FILE);

        record_attempt(&path);

        assert!(cache_is_fresh(&path));
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert!(written.get("latest").is_none(), "{written}");
        show_cached_result(&path).expect("a record with no version is readable");
    }

    /// The spec check downloads the whole document, so its gate is what keeps a
    /// full download off the end of every command.
    #[test]
    fn the_spec_check_gate_expires_with_the_interval() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("api-check-test.json");

        assert!(
            !cache_is_fresh(&marker),
            "no marker means the check has never run"
        );

        write_api_check_marker(&marker, "https://api.ilert.com", "v2.2026.5");
        assert!(cache_is_fresh(&marker));

        std::fs::File::options()
            .write(true)
            .open(&marker)
            .expect("reopen")
            .set_modified(SystemTime::now() - CHECK_INTERVAL - Duration::from_secs(60))
            .expect("backdate");
        assert!(!cache_is_fresh(&marker), "the interval expires");
    }

    /// One marker per environment. Sharing one meant every switch between two
    /// profiles looked like a first check to both, and downloaded the whole
    /// spec again each time.
    #[test]
    fn each_environment_gets_its_own_spec_check_marker() {
        let a = api_check_file_path("https://api.ilert.com").expect("path");
        let b = api_check_file_path("https://api.eu.ilert.com").expect("path");
        assert_ne!(a, b);
        assert_eq!(
            a,
            api_check_file_path("https://api.ilert.com").expect("path"),
            "the name has to be stable across runs, or the interval never holds"
        );
        for path in [&a, &b] {
            let name = path.file_name().and_then(|n| n.to_str()).expect("name");
            assert!(name.starts_with(API_CHECK_PREFIX), "{name}");
            assert!(name.ends_with(".json"), "{name}");
        }
    }

    /// clap prints the binary name itself, so the two blocks differ by exactly
    /// that word — getting it wrong renders "ilert ilert 0.3.0".
    #[test]
    fn the_two_version_blocks_differ_only_by_the_binary_name() {
        let _colors = crate::testutil::colors(false);
        // A base URL with nothing cached for it, so both reduce to their lead
        // line and the comparison is about the prefix alone.
        let unknown = "https://nothing.cached.invalid";
        assert_eq!(
            version_block(unknown),
            format!("ilert {}", current_version())
        );
        assert_eq!(clap_version_block(unknown), current_version());
    }

    /// The spec version is the server's free-form string, and the block wraps it
    /// in colour — so the escaping has to survive that framing. `--version` is
    /// also the one output a script is most likely to read back.
    #[test]
    fn a_hostile_spec_version_cannot_escape_the_version_block() {
        let _colors = crate::testutil::colors(false);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spec.json");
        // Escaped in the JSON, so what lands in the string is the real control
        // character rather than a description of one.
        std::fs::write(
            &path,
            r#"{"info":{"version":"1.0\u001b]52;c;cm0gLXJmIC8=\u0007\nilert 9.9.9"}}"#,
        )
        .expect("write");

        let read = read_spec_version(&path).expect("a version is present");
        assert!(read.contains('\u{1b}'), "the fixture has to be hostile");

        let rendered = format!("{}   {}", "api".dimmed(), terminal_text(&read).cyan());
        for forbidden in ['\u{1b}', '\u{7}', '\n'] {
            assert!(
                !rendered.contains(forbidden),
                "{forbidden:?} survived into {rendered:?}"
            );
        }
    }

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
