mod helpers;

use helpers::TestHarness;
use predicates::prelude::*;
use serde_json::json;
use wiremock::matchers::{
    bearer_token, body_json, body_string_contains, header, method, path, path_regex, query_param,
};
use wiremock::{Mock, ResponseTemplate};

// ---------------------------------------------------------------------------
// Spec loading & command discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spec_is_fetched_and_cached_on_first_run() {
    let h = TestHarness::start().await;

    // First run: fetches spec, shows ops
    h.cmd()
        .args(["ops", "list", "--tag", "alerts"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alerts"));

    // Verify spec was requested
    h.verify_spec_fetched().await;
}

#[tokio::test]
async fn help_shows_dynamic_commands_after_spec_cached() {
    let h = TestHarness::start().await;

    // Seed the cache
    h.cmd().args(["ops", "list"]).assert().success();

    // Now --help should include dynamic commands from the spec
    h.cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("alerts"))
        .stdout(predicate::str::contains("incidents"))
        .stdout(predicate::str::contains("services"));
}

#[tokio::test]
async fn subcommand_help_shows_actions_from_spec() {
    let h = TestHarness::start().await;
    h.cmd().args(["ops", "list"]).assert().success();

    h.cmd()
        .args(["alerts", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("get"))
        .stdout(predicate::str::contains("create"));
}

#[tokio::test]
async fn action_help_shows_parameters_from_spec() {
    let h = TestHarness::start().await;
    h.cmd().args(["ops", "list"]).assert().success();

    h.cmd()
        .args(["alerts", "list", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--states"))
        .stdout(predicate::str::contains("--max-results"))
        .stdout(predicate::str::contains("--all"));
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_login_stores_credentials() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["auth", "login", "--api-key", "test-key-12345"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Logged in"));

    // auth show should display the masked key
    h.cmd()
        .args(["auth", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("test...2345"));
}

#[tokio::test]
async fn auth_whoami_calls_current_user() {
    let h = TestHarness::start().await;

    Mock::given(method("GET"))
        .and(path("/api/users/current"))
        .and(bearer_token("my-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 1,
            "username": "testuser",
            "firstName": "Test",
            "lastName": "User",
            "email": "test@ilert.com"
        })))
        .mount(h.server())
        .await;

    h.cmd()
        .args(["auth", "whoami", "--api-key", "my-api-key"])
        .assert()
        .success()
        .stdout(predicate::str::contains("testuser"));
}

#[tokio::test]
async fn auth_logout_removes_key() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["auth", "login", "--api-key", "test-key-12345"])
        .assert()
        .success();

    h.cmd()
        .args(["auth", "logout"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Logged out"));

    h.cmd()
        .args(["auth", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not set"));
}

#[tokio::test]
async fn auth_login_oauth_browser_flow() {
    let h = TestHarness::start().await;

    // Token endpoint exchanges the (injected) code for OAuth tokens.
    Mock::given(method("POST"))
        .and(path("/api/developers/oauth2/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token_type": "Bearer",
            "access_token": "oauth-access-token",
            "expires_in": 3600,
            "scope": "wildcard:d offline_access",
            "refresh_token": "oauth-refresh-token",
            "refresh_token_expires_in": 31536000
        })))
        .mount(h.server())
        .await;

    // No creds flag => OAuth flow. The test seam injects the auth code so the
    // browser/loopback steps are skipped.
    h.cmd()
        .env("ILERT_OAUTH_TEST_CODE", "test-auth-code")
        .args(["auth", "login"])
        .assert()
        .success()
        .stderr(predicate::str::contains("via OAuth"));

    // Stored credential is OAuth with the granted scopes, and the refresh token
    // from the response is actually persisted — without it the session dies at
    // the access token's TTL and the user is silently logged out.
    h.cmd()
        .args(["auth", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("oauth"))
        .stdout(predicate::str::contains("wildcard"))
        .stdout(predicate::str::contains("present"));

    // The access token is used as the Bearer for API calls.
    Mock::given(method("GET"))
        .and(path("/api/users/current"))
        .and(bearer_token("oauth-access-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 1,
            "username": "oauthuser"
        })))
        .mount(h.server())
        .await;

    h.cmd()
        .args(["auth", "whoami"])
        .assert()
        .success()
        .stdout(predicate::str::contains("oauthuser"));
}

#[tokio::test]
async fn oauth_access_token_is_refreshed_when_expired() {
    let h = TestHarness::start().await;

    // Seed an already-expired OAuth credential for the default profile.
    h.seed_secret(
        "default",
        json!({
            "type": "oauth",
            "access_token": "stale-access-token",
            "refresh_token": "old-refresh-token",
            "expires_at": "2000-01-01T00:00:00Z",
            "token_type": "Bearer",
            "scopes": ["wildcard:d"]
        }),
    );

    // Refresh exchange returns a fresh access token (and rotates the refresh token).
    Mock::given(method("POST"))
        .and(path("/api/developers/oauth2/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token_type": "Bearer",
            "access_token": "fresh-access-token",
            "expires_in": 3600,
            "scope": "wildcard:d offline_access",
            "refresh_token": "new-refresh-token",
            "refresh_token_expires_in": 31536000
        })))
        .mount(h.server())
        .await;

    // The API call must carry the *refreshed* access token.
    Mock::given(method("GET"))
        .and(path("/api/users/current"))
        .and(bearer_token("fresh-access-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 7,
            "username": "refresheduser"
        })))
        .mount(h.server())
        .await;

    h.cmd()
        .args(["auth", "whoami"])
        .assert()
        .success()
        .stdout(predicate::str::contains("refresheduser"));
}

#[tokio::test]
async fn auth_login_with_token_reads_stdin() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["auth", "login", "--with-token"])
        .write_stdin("stdin-key-456\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("Logged in"));

    Mock::given(method("GET"))
        .and(path("/api/users/current"))
        .and(bearer_token("stdin-key-456"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 2,
            "username": "tokenuser"
        })))
        .mount(h.server())
        .await;

    h.cmd()
        .args(["auth", "whoami"])
        .assert()
        .success()
        .stdout(predicate::str::contains("tokenuser"));
}

// ---------------------------------------------------------------------------
// List operations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_alerts_sends_get_request() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .and(bearer_token("test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "summary": "Server down", "status": "PENDING"},
            {"id": 2, "summary": "High CPU", "status": "ACCEPTED"},
        ])))
        .mount(h.server())
        .await;

    h.cmd()
        .args(["alerts", "list", "--api-key", "test-key", "-o", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Server down"))
        .stdout(predicate::str::contains("High CPU"));
}

#[tokio::test]
async fn list_alerts_passes_query_params() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .and(query_param("states", "PENDING"))
        .and(query_param("max-results", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "summary": "Filtered alert", "status": "PENDING"},
        ])))
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "alerts",
            "list",
            "--api-key",
            "test-key",
            "--states",
            "PENDING",
            "--max-results",
            "10",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Filtered alert"));
}

// ---------------------------------------------------------------------------
// Get single resource
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_alert_by_id() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts/42"))
        .and(bearer_token("test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 42,
            "summary": "Database connection lost",
            "status": "PENDING",
            "priority": "HIGH"
        })))
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "alerts",
            "get",
            "--id",
            "42",
            "--api-key",
            "test-key",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Database connection lost"))
        .stdout(predicate::str::contains("PENDING"));
}

// ---------------------------------------------------------------------------
// Create with --set
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_with_set_flags_sends_json_body() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("POST"))
        .and(path("/api/incidents"))
        .and(bearer_token("test-key"))
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 99,
            "summary": "Deploy failure",
            "status": "INVESTIGATING",
        })))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "incidents",
            "create",
            "--api-key",
            "test-key",
            "--set",
            "summary=Deploy failure",
            "--set",
            "status=INVESTIGATING",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Deploy failure"));
}

// ---------------------------------------------------------------------------
// Create with --body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_with_body_flag_sends_raw_json() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("POST"))
        .and(path("/api/incidents"))
        .and(bearer_token("test-key"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 100,
            "summary": "Full body test",
        })))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "incidents",
            "create",
            "--api-key",
            "test-key",
            "--body",
            r#"{"summary":"Full body test","status":"INVESTIGATING"}"#,
            "-o",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Full body test"));
}

// ---------------------------------------------------------------------------
// Dry-run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dry_run_does_not_send_request() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    // Mount a mock that should NOT be called
    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(h.server())
        .await;

    let out = h
        .cmd()
        .args([
            "alerts",
            "list",
            "--api-key",
            "test-key",
            "--dry-run",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("dry run emits JSON on stdout");

    assert_eq!(envelope["status"], "dry_run");
    assert_eq!(envelope["request"]["method"], "GET");
    assert_eq!(envelope["classification"]["read_only"], true);
    assert_eq!(envelope["confirmation"]["required"], false);
    assert_eq!(envelope["confirmation"]["flag"], "--yes");

    // The envelope is the whole output: no human-readable line alongside it.
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "",
        "dry run must not print a diagnostic line"
    );
}

#[tokio::test]
async fn dry_run_emits_the_envelope_in_table_mode_too() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    let out = h
        .cmd()
        .args(["alerts", "list", "--api-key", "test-key", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .clone();

    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the envelope ignores -o");
    assert_eq!(envelope["status"], "dry_run");
}

#[tokio::test]
async fn dry_run_needs_no_credentials() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    h.cmd()
        .args(["alerts", "list", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry_run"));
}

// ---------------------------------------------------------------------------
// Mode detection and the confirmation gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_destructive_command_is_refused_when_nothing_can_confirm_it() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("DELETE"))
        .and(path("/api/alert-sources/42"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(h.server())
        .await;

    let out = h
        .cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args([
            "alert-sources",
            "delete",
            "--id",
            "42",
            "--api-key",
            "test-key",
        ])
        .assert()
        .failure()
        .code(2)
        .get_output()
        .clone();

    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stderr).expect("a refusal is JSON on stderr");

    assert_eq!(envelope["status"], "confirmation_required");
    assert_eq!(envelope["confirmation"]["required"], true);
    assert_eq!(envelope["confirmation"]["flag"], "--yes");
    assert_eq!(envelope["classification"]["destructive"], true);
    assert_eq!(envelope["request"]["method"], "DELETE");
    assert!(
        envelope["request"]["url"]
            .as_str()
            .unwrap()
            .ends_with("/api/alert-sources/42"),
        "the refusal describes the exact request it refused"
    );
    // No reconstructed shell command, and no credential anywhere in it.
    let raw = String::from_utf8_lossy(&out.stderr);
    assert!(!raw.contains("confirmCommand"));
    assert!(!raw.contains("test-key"));
}

#[tokio::test]
async fn yes_lets_a_destructive_command_through() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("DELETE"))
        .and(path("/api/alert-sources/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args([
            "alert-sources",
            "delete",
            "--id",
            "42",
            "--api-key",
            "test-key",
            "--yes",
        ])
        .assert()
        .success();
}

#[tokio::test]
async fn dry_run_beats_the_confirmation_gate() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("DELETE"))
        .and(path("/api/alert-sources/42"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(h.server())
        .await;

    h.cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args(["alert-sources", "delete", "--id", "42", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry_run"));
}

#[tokio::test]
async fn an_unknown_mode_is_a_usage_error() {
    let h = TestHarness::start().await;

    h.cmd()
        .env("ILERT_CLI_MODE", "sideways")
        .args(["version"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("ILERT_CLI_MODE"));
}

// ---------------------------------------------------------------------------
// Raw path passthrough: ilert api
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_sends_a_request_to_a_raw_path() {
    let h = TestHarness::start().await;

    Mock::given(method("GET"))
        .and(path("/api/anything-new"))
        .and(bearer_token("test-key"))
        .and(query_param("limit", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "api",
            "/api/anything-new?limit=5",
            "--api-key",
            "test-key",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[tokio::test]
async fn api_builds_a_json_body_from_field_flags() {
    let h = TestHarness::start().await;

    Mock::given(method("POST"))
        .and(path("/api/alerts"))
        .and(body_json(
            json!({"summary": "Test", "priority": "HIGH", "count": 5}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "api",
            "/api/alerts",
            "-X",
            "POST",
            "-F",
            "summary=Test",
            "-F",
            "priority:=\"HIGH\"",
            "-F",
            "count:=5",
            "--api-key",
            "test-key",
        ])
        .assert()
        .success();
}

#[tokio::test]
async fn api_reads_a_body_from_stdin() {
    let h = TestHarness::start().await;

    Mock::given(method("POST"))
        .and(path("/api/alerts"))
        .and(body_json(json!({"summary": "New"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "api",
            "/api/alerts",
            "-X",
            "POST",
            "--input",
            "-",
            "--api-key",
            "test-key",
        ])
        .write_stdin(r#"{"summary":"New"}"#)
        .assert()
        .success();
}

#[tokio::test]
async fn api_include_prints_the_status_and_headers() {
    let h = TestHarness::start().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-request-id", "abc123")
                .set_body_json(json!([])),
        )
        .mount(h.server())
        .await;

    h.cmd()
        .args(["api", "/api/alerts", "-i", "--api-key", "test-key"])
        .assert()
        .success()
        .stdout(predicate::str::contains("HTTP 200"))
        .stdout(predicate::str::contains("x-request-id: abc123"));
}

#[tokio::test]
async fn api_refuses_to_leave_the_configured_origin() {
    let h = TestHarness::start().await;

    for target in [
        "https://evil.example/api/alerts",
        "//evil.example/api/alerts",
    ] {
        h.cmd()
            .args(["api", target, "--api-key", "test-key"])
            .assert()
            .failure()
            .code(1);
    }
}

#[tokio::test]
async fn api_refuses_a_reserved_header() {
    let h = TestHarness::start().await;

    h.cmd()
        .args([
            "api",
            "/api/alerts",
            "-H",
            "Authorization: Bearer stolen",
            "--api-key",
            "test-key",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("reserved"));
}

#[tokio::test]
async fn a_custom_header_reaches_the_request() {
    let h = TestHarness::start().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .and(header("x-request-id", "from-cli"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "api",
            "/api/alerts",
            "-H",
            "x-request-id: from-cli",
            "--api-key",
            "test-key",
        ])
        .assert()
        .success();
}

#[tokio::test]
async fn api_dry_run_previews_without_sending() {
    let h = TestHarness::start().await;

    Mock::given(method("DELETE"))
        .and(path("/api/alert-sources/7"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(h.server())
        .await;

    let out = h
        .cmd()
        .args(["api", "/api/alert-sources/7", "-X", "DELETE", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .clone();

    let envelope: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON envelope");
    assert_eq!(envelope["status"], "dry_run");
    assert_eq!(envelope["classification"]["destructive"], true);
}

#[tokio::test]
async fn api_still_accepts_an_operation_id_with_a_deprecation_warning() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args(["api", "get-alerts", "--api-key", "test-key"])
        .assert()
        .success()
        .stderr(predicate::str::contains("ilert ops run get-alerts"));
}

// ---------------------------------------------------------------------------
// ops run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ops_run_executes_an_operation_by_id() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 42})))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "ops",
            "run",
            "get-alerts-id",
            "--param",
            "id=42",
            "--api-key",
            "test-key",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}

#[tokio::test]
async fn ops_list_reports_classification() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["ops", "list", "--tag", "alerts", "-o", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("classification"))
        .stdout(predicate::str::contains("read_only"));
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

#[tokio::test]
async fn skills_list_names_the_bundled_skills() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["skills", "list", "-o", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("migrate-from-pagerduty"))
        .stdout(predicate::str::contains("migrate-from-opsgenie"));
}

#[tokio::test]
async fn skills_show_prints_raw_markdown_and_writes_nothing() {
    let h = TestHarness::start().await;

    let before = std::fs::read_dir(h.secret_file().parent().unwrap())
        .unwrap()
        .count();

    h.cmd()
        .args(["skills", "show", "migrate-from-pagerduty"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("---"))
        .stdout(predicate::str::contains("name: migrate-from-pagerduty"));

    let after = std::fs::read_dir(h.secret_file().parent().unwrap())
        .unwrap()
        .count();
    assert_eq!(before, after, "skills show must not write to disk");
}

#[tokio::test]
async fn an_unknown_skill_lists_what_is_available() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["skills", "show", "migrate-from-nagios"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("migrate-from-pagerduty"));
}

// ---------------------------------------------------------------------------
// Client identity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_request_identifies_the_cli() {
    let h = TestHarness::start().await;

    let expected = format!("ilert-cli/{}", env!("CARGO_PKG_VERSION"));

    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .and(header("user-agent", expected.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args(["api", "/api/alerts", "--api-key", "test-key"])
        .assert()
        .success();
}

#[tokio::test]
async fn a_retried_request_keeps_the_user_agent() {
    let h = TestHarness::start().await;

    let expected = format!("ilert-cli/{}", env!("CARGO_PKG_VERSION"));

    // First call 503, then success — both must carry the identity header.
    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .and(header("user-agent", expected.as_str()))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .expect(1)
        .mount(h.server())
        .await;

    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .and(header("user-agent", expected.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args(["api", "/api/alerts", "--api-key", "test-key"])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// Pagination with --all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_flag_paginates_through_pages() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    // Page 1: 2 items (full page with page_size=2)
    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .and(query_param("start-index", "0"))
        .and(query_param("max-results", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "summary": "Alert one"},
            {"id": 2, "summary": "Alert two"},
        ])))
        .expect(1)
        .mount(h.server())
        .await;

    // Page 2: 1 item (partial page = last page)
    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .and(query_param("start-index", "2"))
        .and(query_param("max-results", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 3, "summary": "Alert three"},
        ])))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "alerts",
            "list",
            "--api-key",
            "test-key",
            "--all",
            "--max-results",
            "2",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Alert one"))
        .stdout(predicate::str::contains("Alert two"))
        .stdout(predicate::str::contains("Alert three"));
}

// ---------------------------------------------------------------------------
// Output formats
// ---------------------------------------------------------------------------

#[tokio::test]
async fn output_json_format() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("GET"))
        .and(path("/api/services"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "API Gateway", "status": "OPERATIONAL"}
        ])))
        .mount(h.server())
        .await;

    let output = h
        .cmd()
        .args(["services", "list", "--api-key", "k", "-o", "json"])
        .output()
        .expect("failed to run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("invalid JSON output");
    assert!(parsed.is_array());
    assert_eq!(parsed[0]["name"], "API Gateway");
}

#[tokio::test]
async fn output_ndjson_format() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("GET"))
        .and(path("/api/services"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "Web"},
            {"id": 2, "name": "DB"},
        ])))
        .mount(h.server())
        .await;

    let output = h
        .cmd()
        .args(["services", "list", "--api-key", "k", "-o", "ndjson"])
        .output()
        .expect("failed to run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2);
    // Each line should be valid JSON
    for line in &lines {
        serde_json::from_str::<serde_json::Value>(line).expect("line is not valid JSON");
    }
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_error_is_reported() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts/999"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "message": "Alert not found",
            "code": "NOT_FOUND"
        })))
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "alerts",
            "get",
            "--id",
            "999",
            "--api-key",
            "test-key",
            "-o",
            "json",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Alert not found"));
}

#[tokio::test]
async fn missing_api_key_returns_auth_error() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    h.cmd()
        .args(["alerts", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Not authenticated"));
}

// ---------------------------------------------------------------------------
// Config profiles
// ---------------------------------------------------------------------------

#[tokio::test]
async fn config_show_displays_paths() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["config", "show", "-o", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("config_path"))
        .stdout(predicate::str::contains("cache_dir"));
}

#[tokio::test]
async fn config_list_shows_profiles_after_login() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["auth", "login", "--api-key", "key1"])
        .assert()
        .success();

    h.cmd()
        .args(["config", "list", "-o", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("default"));
}

// ---------------------------------------------------------------------------
// Ops discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ops_list_shows_all_operations() {
    let h = TestHarness::start().await;

    let output = h
        .cmd()
        .args(["ops", "list", "-o", "json"])
        .output()
        .expect("failed to run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("invalid JSON");
    let ops = parsed.as_array().expect("should be array");

    // Real spec should have many operations
    assert!(ops.len() > 50, "Expected 50+ operations, got {}", ops.len());

    // Check structure
    let first = &ops[0];
    assert!(first.get("id").is_some());
    assert!(first.get("tag").is_some());
    assert!(first.get("action").is_some());
    assert!(first.get("method").is_some());
    assert!(first.get("path").is_some());
}

#[tokio::test]
async fn ops_show_displays_operation_details() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["ops", "show", "get-alerts", "-o", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("/api/alerts"))
        .stdout(predicate::str::contains("GET"));
}

// ---------------------------------------------------------------------------
// Nested --set values
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_supports_nested_keys() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("POST"))
        .and(path("/api/incidents"))
        .and(bearer_token("test-key"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(h.server())
        .await;

    // --set with nested keys should work
    h.cmd()
        .args([
            "incidents",
            "create",
            "--api-key",
            "test-key",
            "--set",
            "summary=Test",
            "--set",
            "status=INVESTIGATING",
            "-o",
            "json",
        ])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// Completions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn completions_generate_for_bash() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("complete"));
}

#[tokio::test]
async fn completions_generate_for_zsh() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["completions", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("compdef"));
}

// ---------------------------------------------------------------------------
// Event send
// ---------------------------------------------------------------------------

#[tokio::test]
async fn event_send_posts_to_events_api() {
    let h = TestHarness::start().await;

    Mock::given(method("POST"))
        .and(path("/api/events"))
        .and(body_json(json!({
            "integrationKey": "il1-int-test123",
            "eventType": "ALERT",
            "summary": "Server on fire",
        })))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "alertKey": "abc123",
            "eventType": "ALERT",
        })))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "event",
            "send",
            "--integration-key",
            "il1-int-test123",
            "--summary",
            "Server on fire",
            "--api-key",
            "unused",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("abc123"));
}

#[tokio::test]
async fn event_send_with_custom_details() {
    let h = TestHarness::start().await;

    Mock::given(method("POST"))
        .and(path("/api/events"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({"alertKey": "x"})))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "event",
            "send",
            "--integration-key",
            "key1",
            "--summary",
            "Deploy failed",
            "--type",
            "ALERT",
            "--custom",
            "env=prod",
            "--custom",
            "region=eu",
            "--api-key",
            "unused",
            "-o",
            "json",
        ])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// Alert aliases (ack/resolve)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn alert_ack_alias_sends_accept_request() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("PUT"))
        .and(path("/api/alerts/42/accept"))
        .and(bearer_token("my-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 42, "status": "ACCEPTED"
        })))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args(["alerts", "ack", "42", "--api-key", "my-key", "-o", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ACCEPTED"))
        .stderr(predicate::str::contains("accepted"));
}

#[tokio::test]
async fn alert_resolve_alias_sends_resolve_request() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("PUT"))
        .and(path("/api/alerts/99/resolve"))
        .and(bearer_token("my-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 99, "status": "RESOLVED"
        })))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "alerts",
            "resolve",
            "99",
            "--api-key",
            "my-key",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("RESOLVED"))
        .stderr(predicate::str::contains("resolved"));
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn version_command_shows_version() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["version"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ilert 0.1.0"));
}

// ---------------------------------------------------------------------------
// Status overview
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_command_fetches_overview() {
    let h = TestHarness::start().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts/count"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"count": 3})))
        .mount(h.server())
        .await;

    Mock::given(method("GET"))
        .and(path("/api/incidents"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(h.server())
        .await;

    Mock::given(method("GET"))
        .and(path("/api/services"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "API", "status": "OPERATIONAL"},
            {"id": 2, "name": "DB", "status": "DEGRADED"},
        ])))
        .mount(h.server())
        .await;

    h.cmd()
        .args(["status", "--api-key", "test-key", "-o", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alerts"))
        .stdout(predicate::str::contains("services"));
}

// ---------------------------------------------------------------------------
// Config import
// ---------------------------------------------------------------------------

#[tokio::test]
async fn config_import_from_env() {
    let h = TestHarness::start().await;

    h.cmd()
        .env("ILERT_API_KEY", "imported-key-123")
        .env("ILERT_BASE_URL", "https://custom.ilert.com")
        .args(["config", "import"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Imported"));

    // Verify the key was imported
    h.cmd()
        .args(["auth", "show", "-o", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("impo...-123"));
}

// ---------------------------------------------------------------------------
// Help for new commands
// ---------------------------------------------------------------------------

#[tokio::test]
async fn heartbeat_ping_help() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["heartbeat", "ping", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("integration key"));
}

#[tokio::test]
async fn on_call_help() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["on-call", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("on call"));
}

#[tokio::test]
async fn dashboard_shows_in_help() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dashboard"));
}

// ---------------------------------------------------------------------------
// Execution safety: --stdin, --watch and --dry-run all pass the same gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stdin_delete_without_yes_sends_nothing_and_exits_two() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("DELETE"))
        .and(path_regex(r"^/api/alert-sources/\d+$"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(h.server())
        .await;

    let out = h
        .cmd()
        .env("ILERT_CLI_MODE", "agent")
        .write_stdin("41\n42\n43\n")
        .args([
            "alert-sources",
            "delete",
            "--stdin",
            "--api-key",
            "test-key",
        ])
        .assert()
        .failure()
        .code(2)
        .get_output()
        .clone();

    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stderr).expect("a refusal is JSON on stderr");
    assert_eq!(envelope["status"], "confirmation_required");
    assert_eq!(envelope["classification"]["destructive"], true);
    assert_eq!(envelope["request"]["method"], "DELETE");
    // The IDs were never read, so the refusal describes the template it refused.
    assert!(
        envelope["request"]["url"]
            .as_str()
            .unwrap()
            .ends_with("/api/alert-sources/{id}"),
        "got {}",
        envelope["request"]["url"]
    );
}

#[tokio::test]
async fn stdin_delete_with_yes_deletes_every_id() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("DELETE"))
        .and(path_regex(r"^/api/alert-sources/\d+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(3)
        .mount(h.server())
        .await;

    h.cmd()
        .env("ILERT_CLI_MODE", "agent")
        .write_stdin("41\n42\n\n43\n")
        .args([
            "alert-sources",
            "delete",
            "--stdin",
            "--yes",
            "--api-key",
            "test-key",
        ])
        .assert()
        .success();
}

#[tokio::test]
async fn dry_run_with_stdin_sends_nothing() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("DELETE"))
        .and(path_regex(r"^/api/alert-sources/\d+$"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(h.server())
        .await;

    let out = h
        .cmd()
        .env("ILERT_CLI_MODE", "agent")
        .write_stdin("41\n42\n")
        .args(["alert-sources", "delete", "--stdin", "--dry-run", "--yes"])
        .assert()
        .success()
        .get_output()
        .clone();

    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("a dry run is JSON on stdout");
    assert_eq!(envelope["status"], "dry_run");
}

#[tokio::test]
async fn dry_run_with_watch_sends_nothing() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    // A watch loop never returns, so a --dry-run that reached it would hang
    // here rather than fail — which is exactly the regression this guards.
    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(h.server())
        .await;

    h.cmd()
        .timeout(std::time::Duration::from_secs(20))
        .args(["alerts", "list", "--watch", "1", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry_run"));
}

#[tokio::test]
async fn dry_run_never_touches_the_secret_store() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    // No credential anywhere: no --api-key, no env, and a secret file that
    // would fail to parse if anything tried to read it.
    std::fs::write(h.secret_file(), "not json at all").expect("write secret file");

    h.cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args(["alert-sources", "delete", "--id", "42", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry_run"));
}

#[tokio::test]
async fn a_refusal_never_touches_the_secret_store() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    std::fs::write(h.secret_file(), "not json at all").expect("write secret file");

    h.cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args(["alert-sources", "delete", "--id", "42"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("confirmation_required"));
}

#[tokio::test]
async fn an_unknown_raw_method_needs_confirmation() {
    let h = TestHarness::start().await;

    Mock::given(method("PURGE"))
        .and(path("/api/alerts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(0)
        .mount(h.server())
        .await;

    let out = h
        .cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args(["api", "/api/alerts", "-X", "PURGE", "--api-key", "test-key"])
        .assert()
        .failure()
        .code(2)
        .get_output()
        .clone();

    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stderr).expect("a refusal is JSON on stderr");
    assert_eq!(envelope["classification"]["destructive"], true);
    assert_eq!(envelope["request"]["method"], "PURGE");
}

// ---------------------------------------------------------------------------
// Header safety
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_reserved_header_cannot_enter_through_param() {
    let h = TestHarness::start_with_spec(helpers::CLASSIFICATION_SPEC).await;
    h.seed_cache().await;

    // `Authorization` is a declared header parameter on this operation, so it
    // reaches the reserved-header check rather than the unknown-name check.
    h.cmd()
        .args([
            "ops",
            "run",
            "auditVault",
            "--param",
            "id=1",
            "--param",
            "Authorization=Bearer stolen",
            "--api-key",
            "test-key",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("reserved"));

    // Anything the operation does not declare cannot become a header at all.
    for name in ["x-team-context", "Host", "content-length"] {
        h.cmd()
            .args([
                "ops",
                "run",
                "auditVault",
                "--param",
                "id=1",
                "--param",
                &format!("{name}=stolen"),
                "--api-key",
                "test-key",
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains("Unknown --param"));
    }
}

#[tokio::test]
async fn a_reserved_header_parameter_in_the_spec_is_refused() {
    let h = TestHarness::start_with_spec(helpers::CLASSIFICATION_SPEC).await;
    h.seed_cache().await;

    Mock::given(method("GET"))
        .and(path("/api/audits/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "audits",
            "get",
            "--id",
            "1",
            "--Authorization",
            "Bearer stolen",
            "--api-key",
            "test-key",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("reserved"));
}

// ---------------------------------------------------------------------------
// Classification of a destructive non-DELETE operation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_spec_can_mark_a_non_delete_operation_destructive() {
    let h = TestHarness::start_with_spec(helpers::CLASSIFICATION_SPEC).await;
    h.seed_cache().await;

    Mock::given(method("PUT"))
        .and(path("/api/vaults/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(0)
        .mount(h.server())
        .await;

    let out = h
        .cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args([
            "vaults",
            "update",
            "--id",
            "7",
            "--set",
            "key=new",
            "--api-key",
            "test-key",
        ])
        .assert()
        .failure()
        .code(2)
        .get_output()
        .clone();

    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stderr).expect("a refusal is JSON on stderr");
    assert_eq!(envelope["request"]["method"], "PUT");
    assert_eq!(envelope["classification"]["destructive"], true);
    assert_eq!(envelope["classification"]["read_only"], false);
}

#[tokio::test]
async fn the_same_operation_proceeds_with_yes() {
    let h = TestHarness::start_with_spec(helpers::CLASSIFICATION_SPEC).await;
    h.seed_cache().await;

    Mock::given(method("PUT"))
        .and(path("/api/vaults/7"))
        .and(body_json(json!({"key": "new"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args([
            "vaults",
            "update",
            "--id",
            "7",
            "--set",
            "key=new",
            "--yes",
            "--api-key",
            "test-key",
        ])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// Request validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_required_query_parameter_is_rejected_before_sending() {
    let h = TestHarness::start_with_spec(helpers::CLASSIFICATION_SPEC).await;
    h.seed_cache().await;

    Mock::given(method("GET"))
        .and(path("/api/vaults"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(h.server())
        .await;

    h.cmd()
        .args(["ops", "run", "listVaults", "--api-key", "test-key"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("since"));
}

#[tokio::test]
async fn a_required_header_parameter_is_rejected_before_sending() {
    let h = TestHarness::start_with_spec(helpers::CLASSIFICATION_SPEC).await;
    h.seed_cache().await;

    Mock::given(method("POST"))
        .and(path("/api/vaults/1/lock"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(0)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "ops",
            "run",
            "lockVault",
            "--param",
            "id=1",
            "--api-key",
            "test-key",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("If-Match"));
}

#[tokio::test]
async fn a_required_body_is_rejected_before_previewing() {
    let h = TestHarness::start_with_spec(helpers::CLASSIFICATION_SPEC).await;
    h.seed_cache().await;

    // Even --dry-run refuses: an incomplete request is not worth previewing.
    h.cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args(["ops", "run", "rekeyVault", "--param", "id=7", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("request body"));
}

#[tokio::test]
async fn an_unknown_param_is_a_usage_error() {
    let h = TestHarness::start_with_spec(helpers::CLASSIFICATION_SPEC).await;
    h.seed_cache().await;

    h.cmd()
        .args([
            "ops",
            "run",
            "listVaults",
            "--param",
            "sicne=2024-01-01",
            "--api-key",
            "test-key",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Unknown --param 'sicne'"))
        .stderr(predicate::str::contains("since"));
}

#[tokio::test]
async fn a_malformed_param_is_a_usage_error() {
    let h = TestHarness::start_with_spec(helpers::CLASSIFICATION_SPEC).await;
    h.seed_cache().await;

    h.cmd()
        .args([
            "ops",
            "run",
            "listVaults",
            "--param",
            "since",
            "--api-key",
            "test-key",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("NAME=VALUE"));
}

// ---------------------------------------------------------------------------
// Raw responses keep their metadata on failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_include_keeps_metadata_on_an_error_response() {
    let h = TestHarness::start().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts/999"))
        .respond_with(
            ResponseTemplate::new(404)
                .insert_header("x-request-id", "abc123")
                .set_body_json(json!({"message": "not found"})),
        )
        .mount(h.server())
        .await;

    h.cmd()
        .args(["api", "/api/alerts/999", "-i", "--api-key", "test-key"])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("HTTP 404"))
        .stdout(predicate::str::contains("x-request-id: abc123"))
        .stdout(predicate::str::contains("not found"));
}

#[tokio::test]
async fn api_verbose_reports_request_headers_without_leaking_credentials() {
    let h = TestHarness::start().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(h.server())
        .await;

    let out = h
        .cmd()
        .args([
            "api",
            "/api/alerts",
            "--verbose",
            "-H",
            "x-request-id: from-cli",
            "--api-key",
            "il1api-secret",
            "--team-context",
            "99",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Authorization"), "{stderr}");
    assert!(stderr.contains("x-team-context"), "{stderr}");
    assert!(stderr.contains("x-request-id"), "{stderr}");
    assert!(stderr.contains("User-Agent"), "{stderr}");
    assert!(!stderr.contains("il1api-secret"), "{stderr}");
    assert!(!stderr.contains(": 99"), "{stderr}");
}

// ---------------------------------------------------------------------------
// Global --jq
// ---------------------------------------------------------------------------

#[tokio::test]
async fn jq_filters_a_dynamic_command() {
    if !jq_available() {
        return;
    }
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"id": 1, "summary": "one"}, {"id": 2, "summary": "two"}])),
        )
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "alerts",
            "list",
            "--jq",
            ".[].summary",
            "--api-key",
            "test-key",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("one"))
        .stdout(predicate::str::contains("two"));
}

#[tokio::test]
async fn jq_filters_every_static_command_that_prints_data() {
    if !jq_available() {
        return;
    }
    let h = TestHarness::start().await;

    Mock::given(method("POST"))
        .and(path("/api/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "evt-1"})))
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "event", "send", "-k", "int-key", "-s", "boom", "--jq", ".id",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("evt-1"));

    Mock::given(method("GET"))
        .and(path("/api/alerts/count"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"count": 7})))
        .mount(h.server())
        .await;
    Mock::given(method("GET"))
        .and(path("/api/incidents"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(h.server())
        .await;
    Mock::given(method("GET"))
        .and(path("/api/services"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(h.server())
        .await;

    h.cmd()
        .args(["status", "--jq", ".alerts.count", "--api-key", "test-key"])
        .assert()
        .success()
        .stdout(predicate::str::contains("7"));

    Mock::given(method("GET"))
        .and(path("/api/on-calls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": 5}])))
        .mount(h.server())
        .await;

    h.cmd()
        .args(["on-call", "now", "--jq", ".[].id", "--api-key", "test-key"])
        .assert()
        .success()
        .stdout(predicate::str::contains("5"));

    Mock::given(method("GET"))
        .and(path("/api/pings/hb-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "heartbeat",
            "ping",
            "hb-key",
            "--beat-url",
            &format!("{}/api/pings", h.base_url()),
            "--jq",
            ".status",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[tokio::test]
async fn jq_on_a_non_json_response_is_an_error() {
    if !jq_available() {
        return;
    }
    let h = TestHarness::start().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .respond_with(ResponseTemplate::new(200).set_body_string("plain text, not JSON"))
        .mount(h.server())
        .await;

    h.cmd()
        .args(["api", "/api/alerts", "--jq", ".", "--api-key", "test-key"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("non-JSON"));
}

#[tokio::test]
async fn jq_on_a_skill_document_is_an_error() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["skills", "show", "migrate-from-pagerduty", "--jq", "."])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not JSON"));
}

#[tokio::test]
async fn a_missing_jq_binary_is_reported_clearly() {
    let h = TestHarness::start().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(h.server())
        .await;

    // An empty PATH means the jq binary cannot be found, whatever the machine
    // running the suite happens to have installed.
    h.cmd()
        .env("PATH", "")
        .args(["api", "/api/alerts", "--jq", ".", "--api-key", "test-key"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("jq"));
}

/// The `--jq` tests need the real binary; skip rather than fail where it is absent.
fn jq_available() -> bool {
    std::process::Command::new("jq")
        .arg("--version")
        .output()
        .is_ok()
}

// ---------------------------------------------------------------------------
// --stdin excludes an explicit ID
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stdin_and_an_explicit_id_cannot_be_combined() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    // Whichever of the two won, the other would be silently discarded: either
    // every piped line targets the one ID, or the ID is ignored. Neither is
    // something to guess at, so the combination is refused before anything is
    // sent.
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/api/alert-sources/\d+$"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(h.server())
        .await;

    h.cmd()
        .env("ILERT_CLI_MODE", "agent")
        .write_stdin("41\n42\n")
        .args([
            "alert-sources",
            "delete",
            "--id",
            "7",
            "--stdin",
            "--yes",
            "--api-key",
            "test-key",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

// ---------------------------------------------------------------------------
// jq and JSON string bodies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn jq_filters_a_body_that_is_a_json_string() {
    if !jq_available() {
        return;
    }
    let h = TestHarness::start().await;

    // `"ok"` is valid JSON that happens to decode to a string, exactly like a
    // plain-text body does. Only the first is filterable, and the difference
    // has to survive decoding.
    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("\"ok\"")
                .insert_header("content-type", "application/json"),
        )
        .mount(h.server())
        .await;

    h.cmd()
        .args(["api", "/api/alerts", "--jq", ".", "--api-key", "test-key"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"));
}

#[tokio::test]
async fn jq_filters_a_body_that_is_a_json_number() {
    if !jq_available() {
        return;
    }
    let h = TestHarness::start().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts/count"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("42")
                .insert_header("content-type", "application/json"),
        )
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "api",
            "/api/alerts/count",
            "--jq",
            ". + 1",
            "--api-key",
            "test-key",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("43"));
}

// ---------------------------------------------------------------------------
// Alert aliases go through the same gate as everything else
// ---------------------------------------------------------------------------

#[tokio::test]
async fn alert_aliases_can_be_dry_run() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/api/alerts/\d+/(accept|resolve|assign)$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 42})))
        .expect(0)
        .mount(h.server())
        .await;

    for (args, expect_path) in [
        (
            vec!["alerts", "ack", "42", "--dry-run"],
            "/api/alerts/42/accept",
        ),
        (
            vec!["alerts", "resolve", "42", "--dry-run"],
            "/api/alerts/42/resolve",
        ),
        (
            vec!["alerts", "assign", "42", "--user", "jane", "--dry-run"],
            "/api/alerts/42/assign",
        ),
    ] {
        let out = h
            .cmd()
            .env("ILERT_CLI_MODE", "agent")
            .args(&args)
            .assert()
            .success()
            .get_output()
            .clone();

        let envelope: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("a dry run is JSON on stdout");
        assert_eq!(envelope["status"], "dry_run", "for {args:?}");
        assert_eq!(envelope["request"]["method"], "PUT", "for {args:?}");
        assert!(
            envelope["request"]["url"]
                .as_str()
                .expect("url is a string")
                .ends_with(expect_path),
            "for {args:?}: {}",
            envelope["request"]["url"]
        );
    }
}

#[tokio::test]
async fn an_alert_alias_dry_run_never_touches_the_secret_store() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    // Any attempt to read a credential out of this would fail loudly.
    std::fs::write(h.secret_file(), "not json at all").expect("write secret file");

    h.cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args(["alerts", "resolve", "42", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry_run"));
}

// ---------------------------------------------------------------------------
// No command may silently ignore --jq
// ---------------------------------------------------------------------------

#[tokio::test]
async fn jq_filters_version_output() {
    if !jq_available() {
        return;
    }
    let h = TestHarness::start().await;

    h.cmd()
        .args(["version", "--jq", ".cli"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0.1.0"));
}

#[tokio::test]
async fn version_still_prints_its_two_lines_when_piped() {
    let h = TestHarness::start().await;

    // Reading the version out of a script predates every output flag, and the
    // format silently falls back to JSON off a terminal — so only an explicit
    // request may change this.
    h.cmd()
        .args(["version"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ilert 0.1.0"));
}

#[tokio::test]
async fn jq_on_completions_is_an_error() {
    let h = TestHarness::start().await;

    h.cmd()
        .args(["completions", "bash", "--jq", "."])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not JSON"));
}

#[tokio::test]
async fn jq_on_the_dashboard_is_an_error() {
    let h = TestHarness::start().await;

    // Refused before a credential is resolved, so this never blocks on a
    // keyring or opens a terminal UI in the test runner.
    h.cmd()
        .args(["dashboard", "--jq", "."])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not JSON"));
}

// ---------------------------------------------------------------------------
// A dry run does not fetch the spec either
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_dry_run_does_not_download_the_spec() {
    // Deliberately no seed_cache(): downloading the spec is a network request,
    // and --dry-run promises not to make one.
    let h = TestHarness::start().await;

    h.cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args(["alerts", "get", "--id", "1", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ops list"));

    h.verify_nothing_was_requested().await;
}

#[tokio::test]
async fn ops_run_dry_run_does_not_download_the_spec() {
    let h = TestHarness::start().await;

    h.cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args(["ops", "run", "getAlert", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ops list"));

    h.verify_nothing_was_requested().await;
}

// ---------------------------------------------------------------------------
// Diagnostic output never carries credentials
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dry_run_redacts_sensitive_body_fields() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("POST"))
        .and(path("/api/connectors"))
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
        .mount(h.server())
        .await;

    let out = h
        .cmd()
        .args([
            "ops",
            "run",
            "post-connectors",
            "--body",
            r#"{"name":"smtp","params":{"password":"hunter2","host":"mail.example.com"}}"#,
            "--dry-run",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let rendered = String::from_utf8_lossy(&out.stdout);
    assert!(
        !rendered.contains("hunter2"),
        "the preview leaked a password: {rendered}"
    );

    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("dry run emits JSON on stdout");
    assert_eq!(
        envelope["request"]["body"]["params"]["password"],
        "<redacted>"
    );
    // Redaction is surgical: everything else survives, so the preview still
    // describes the request the caller is about to send.
    assert_eq!(envelope["request"]["body"]["name"], "smtp");
    assert_eq!(
        envelope["request"]["body"]["params"]["host"],
        "mail.example.com"
    );
}

#[tokio::test]
async fn debug_output_redacts_sensitive_body_fields() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("POST"))
        .and(path("/api/connectors"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"id": 9, "params": {"password": "echoed-back"}})),
        )
        .expect(1)
        .mount(h.server())
        .await;

    let out = h
        .cmd()
        .args([
            "ops",
            "run",
            "post-connectors",
            "--body",
            r#"{"name":"smtp","params":{"password":"hunter2"}}"#,
            "--log-level",
            "debug",
            "--api-key",
            "test-key",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("debug:"), "expected debug output: {stderr}");
    assert!(
        !stderr.contains("hunter2"),
        "--debug leaked the request password: {stderr}"
    );
    assert!(
        !stderr.contains("echoed-back"),
        "--debug leaked the response password: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Batch IDs stay inside their path segment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stdin_refuses_an_id_that_would_retarget_the_path() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    // The two well-formed IDs are fetched...
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/alerts/\d+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .expect(2)
        .mount(h.server())
        .await;

    // ...and the traversal never reaches the path it was aiming for.
    Mock::given(method("GET"))
        .and(path("/users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(h.server())
        .await;

    let out = h
        .cmd()
        .env("ILERT_CLI_MODE", "agent")
        .write_stdin("41\n../../users\n42\n")
        .args(["alerts", "get", "--stdin", "--api-key", "test-key"])
        .assert()
        .success()
        .get_output()
        .clone();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("path separator"),
        "expected the rejected line to be reported: {stderr}"
    );
}

#[tokio::test]
async fn stdin_encodes_an_id_that_is_not_url_safe() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    // A pre-encoded separator must arrive as a literal, not as a path boundary.
    Mock::given(method("GET"))
        .and(path("/api/alerts/%252Fetc"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "not found"})))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .env("ILERT_CLI_MODE", "agent")
        .write_stdin("%2Fetc\n")
        .args(["alerts", "get", "--stdin", "--api-key", "test-key"])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// Heartbeat keys are never sent in the clear
// ---------------------------------------------------------------------------

#[tokio::test]
async fn heartbeat_refuses_a_cleartext_beat_url() {
    let h = TestHarness::start().await;

    h.cmd()
        .args([
            "heartbeat",
            "ping",
            "my-heartbeat-key",
            "--beat-url",
            "http://beat.evil.example/api/pings",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--beat-url"))
        .stderr(predicate::str::contains("cleartext"));

    h.verify_nothing_was_requested().await;
}

#[tokio::test]
async fn heartbeat_allows_a_loopback_beat_url() {
    let h = TestHarness::start().await;

    Mock::given(method("GET"))
        .and(path("/api/pings/my-heartbeat-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "heartbeat",
            "ping",
            "my-heartbeat-key",
            "--beat-url",
            &format!("{}/api/pings", h.server().uri()),
        ])
        .assert()
        .success();
}

#[tokio::test]
async fn a_failed_heartbeat_ping_does_not_name_the_key() {
    let h = TestHarness::start().await;

    // Port 9 (discard) with nothing listening: a plain transport failure, the
    // shape a flaky network produces in CI every day. reqwest's own message
    // would quote the whole URL, and the key is the last segment of it.
    h.cmd()
        .args([
            "heartbeat",
            "ping",
            "customer-heartbeat-secret",
            "--beat-url",
            "http://127.0.0.1:9",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Heartbeat ping to 127.0.0.1 failed",
        ))
        .stderr(predicate::str::contains("customer-heartbeat-secret").not());
}

#[tokio::test]
async fn a_heartbeat_redirect_is_not_followed() {
    let h = TestHarness::start().await;
    let sink = TestHarness::start().await;

    Mock::given(method("GET"))
        .and(path("/api/pings/customer-heartbeat-secret"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            format!("{}/api/pings/customer-heartbeat-secret", sink.base_url()).as_str(),
        ))
        .mount(h.server())
        .await;

    h.cmd()
        .args([
            "heartbeat",
            "ping",
            "customer-heartbeat-secret",
            "--beat-url",
            &format!("{}/api/pings", h.base_url()),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("redirect"));

    // The key is in the path, so following the redirect would have handed it to
    // a host that only had to answer with a Location header to get it.
    let followed = sink
        .server()
        .received_requests()
        .await
        .expect("no requests recorded");
    assert!(
        followed.is_empty(),
        "the ping was replayed at the redirect target: {:?}",
        followed.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// The reviewed writes are gated
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invoking_an_alert_action_requires_confirmation() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("POST"))
        .and(path("/api/alerts/42/actions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"success": true})))
        .expect(0)
        .mount(h.server())
        .await;

    let out = h
        .cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args([
            "ops",
            "run",
            "post-alerts-id-actions",
            "--param",
            "id=42",
            "--body",
            r#"{"alertActionId":1}"#,
            "--api-key",
            "test-key",
            "-o",
            "json",
        ])
        .assert()
        .code(2)
        .get_output()
        .clone();

    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stderr).expect("a refusal emits the envelope on stderr");
    assert_eq!(envelope["status"], "confirmation_required");
    assert_eq!(envelope["classification"]["destructive"], true);
    assert_eq!(envelope["confirmation"]["flag"], "--yes");
}

#[tokio::test]
async fn an_ordinary_create_still_needs_no_confirmation() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("POST"))
        .and(path("/api/alert-sources"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 7})))
        .expect(1)
        .mount(h.server())
        .await;

    h.cmd()
        .env("ILERT_CLI_MODE", "agent")
        .args([
            "ops",
            "run",
            "post-alert-sources",
            "--body",
            r#"{"name":"Prometheus"}"#,
            "--api-key",
            "test-key",
            "-o",
            "json",
        ])
        .assert()
        .success();
}

#[tokio::test]
async fn a_post_that_only_queries_is_reported_read_only() {
    let h = TestHarness::start().await;
    h.seed_cache().await;

    Mock::given(method("POST"))
        .and(path("/api/users/search-email"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(h.server())
        .await;

    let out = h
        .cmd()
        .args([
            "ops",
            "run",
            "post-users-search-email",
            "--body",
            r#"{"email":"john@acme.com"}"#,
            "--dry-run",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("dry run emits JSON on stdout");
    assert_eq!(envelope["classification"]["read_only"], true);
    assert_eq!(envelope["classification"]["destructive"], false);
}

// ---------------------------------------------------------------------------
// Credentials never come from config.json
// ---------------------------------------------------------------------------

/// A key sitting in `config.json` is not a credential source. It used to be one;
/// removing that path means a hand-edited (or pre-release) config file is inert
/// rather than a quiet plaintext fallback.
#[tokio::test]
async fn a_plaintext_key_in_the_config_file_does_not_authenticate() {
    let h = TestHarness::start().await;
    h.write_config(serde_json::json!({
        "default_profile": "default",
        "profiles": { "default": { "api_key": "plaintext-key-from-config" } }
    }));
    h.seed_cache().await;

    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(0)
        .mount(h.server())
        .await;

    h.cmd()
        .args(["alerts", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Not authenticated"));
}

/// A config file carrying a key still parses, and logging in rewrites it without
/// the key — `config.json` is settings only, from here on.
#[tokio::test]
async fn login_rewrites_a_config_file_that_carried_a_plaintext_key() {
    let h = TestHarness::start().await;
    h.write_config(serde_json::json!({
        "default_profile": "default",
        "profiles": { "default": { "api_key": "plaintext-key-from-config" } }
    }));

    h.cmd()
        .args([
            "auth",
            "login",
            "--api-key",
            "keyring-key",
            "--team-context",
            "ops",
        ])
        .assert()
        .success();

    let written: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(h.config_file()).expect("config was written"),
    )
    .expect("config is valid JSON");
    assert!(
        written["profiles"]["default"].get("api_key").is_none(),
        "config.json still carries a credential: {written}"
    );
    assert_eq!(written["profiles"]["default"]["team_context"], "ops");
}

// ---------------------------------------------------------------------------
// Non-production environments
// ---------------------------------------------------------------------------

/// The token response every OAuth exchange in this section replies with.
fn oauth_tokens() -> serde_json::Value {
    json!({
        "token_type": "Bearer",
        "access_token": "env-access-token",
        "expires_in": 3600,
        "scope": "wildcard:d offline_access",
        "refresh_token": "env-refresh-token",
        "refresh_token_expires_in": 31536000
    })
}

#[tokio::test]
async fn login_authorizes_as_the_configured_oauth_application() {
    let h = TestHarness::start().await;

    // The exchange only matches when the CLI sent the id it was given — a
    // production id against another environment is rejected by the real IDP.
    Mock::given(method("POST"))
        .and(path("/api/developers/oauth2/token"))
        .and(body_string_contains("client_id=other-env-client-id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(oauth_tokens()))
        .mount(h.server())
        .await;

    h.cmd()
        .env("ILERT_OAUTH_TEST_CODE", "test-auth-code")
        .args(["auth", "login", "--oauth-client-id", "other-env-client-id"])
        .assert()
        .success()
        .stderr(predicate::str::contains("via OAuth"));

    // Persisted next to the base URL, so later commands on this profile keep
    // reaching the same environment without repeating either flag.
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(h.config_file()).expect("config written"))
            .expect("config is valid JSON");
    assert_eq!(
        written["profiles"]["default"]["oauth_client_id"],
        "other-env-client-id"
    );
}

#[tokio::test]
async fn login_against_production_does_not_pin_the_client_id() {
    let h = TestHarness::start().await;

    Mock::given(method("POST"))
        .and(path("/api/developers/oauth2/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(oauth_tokens()))
        .mount(h.server())
        .await;

    h.cmd()
        .env("ILERT_OAUTH_TEST_CODE", "test-auth-code")
        .args(["auth", "login"])
        .assert()
        .success();

    // Writing the default would freeze today's value into config.json and
    // survive a rotation that ships with the binary.
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(h.config_file()).expect("config written"))
            .expect("config is valid JSON");
    assert!(
        written["profiles"]["default"]
            .get("oauth_client_id")
            .is_none(),
        "default client id was pinned into config.json: {written}"
    );
}

#[tokio::test]
async fn a_silent_refresh_uses_the_profile_oauth_application() {
    let h = TestHarness::start().await;

    // A profile set up against another environment, with an expired token.
    h.write_config(json!({
        "default_profile": "default",
        "profiles": { "default": { "oauth_client_id": "other-env-client-id" } }
    }));
    h.seed_secret(
        "default",
        json!({
            "type": "oauth",
            "access_token": "stale-access-token",
            "refresh_token": "old-refresh-token",
            "expires_at": "2000-01-01T00:00:00Z",
            "token_type": "Bearer",
            "scopes": ["wildcard:d"]
        }),
    );

    // Refresh runs on ordinary commands, so it has to carry the same id the
    // login used — otherwise the session dies at the access token's TTL.
    Mock::given(method("POST"))
        .and(path("/api/developers/oauth2/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("client_id=other-env-client-id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(oauth_tokens()))
        .mount(h.server())
        .await;

    Mock::given(method("GET"))
        .and(path("/api/users/current"))
        .and(bearer_token("env-access-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9,
            "username": "otherenvuser"
        })))
        .mount(h.server())
        .await;

    h.cmd()
        .args(["auth", "whoami"])
        .assert()
        .success()
        .stdout(predicate::str::contains("otherenvuser"));
}

#[tokio::test]
async fn the_spec_cache_is_kept_per_environment() {
    let h = TestHarness::start().await;
    h.seed_cache().await;
    let fetched_before = h.spec_request_count().await;
    assert_eq!(fetched_before, 1, "expected exactly one spec fetch to seed");

    // A second environment sharing the same cache directory, serving a spec
    // with entirely different operations in it.
    let other = wiremock::MockServer::start().await;
    let other_spec: serde_json::Value =
        serde_json::from_str(helpers::CLASSIFICATION_SPEC).expect("fixture is valid JSON");
    Mock::given(method("GET"))
        .and(path("/api-docs/openapi.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(other_spec))
        .mount(&other)
        .await;

    // The other environment must fetch its own spec rather than inherit the
    // one already on disk — the spec decides which commands exist at all.
    h.cmd_for(&other.uri())
        .args(["ops", "list", "-o", "raw"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rekeyVault"));

    // ...and doing so must not evict the first environment's spec.
    h.cmd()
        .args(["ops", "list", "-o", "raw"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rekeyVault").not());
    assert_eq!(
        h.spec_request_count().await,
        fetched_before,
        "the production spec was re-fetched after a command against another environment"
    );
}

// ---------------------------------------------------------------------------
// Credentials are bound to the environment that issued them
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_overridden_endpoint_cannot_borrow_stored_credentials() {
    let h = TestHarness::start().await;
    h.cmd()
        .args(["auth", "login", "--api-key", "production-key"])
        .assert()
        .success();

    // Somewhere else entirely — an endpoint this profile never logged in to.
    let elsewhere = wiremock::MockServer::start().await;

    h.cmd_for(&elsewhere.uri())
        .args(["auth", "whoami"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Refusing to send profile 'default'",
        ))
        .stderr(predicate::str::contains("--profile"));

    // The point of the refusal: the key never reached the other host. Not even
    // the spec fetch, which would have gone out before the credential.
    let leaked = elsewhere
        .received_requests()
        .await
        .expect("no requests recorded");
    assert!(
        leaked.is_empty(),
        "credentials were offered to an unrelated endpoint: {:?}",
        leaked.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn an_explicit_api_key_may_target_any_endpoint() {
    let h = TestHarness::start().await;
    h.cmd()
        .args(["auth", "login", "--api-key", "production-key"])
        .assert()
        .success();

    // A key passed per invocation is the caller's to place: it is never stored,
    // and it is not the profile's credential being redirected.
    let elsewhere = TestHarness::start().await;
    Mock::given(method("GET"))
        .and(path("/api/users/current"))
        .and(bearer_token("throwaway-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 3,
            "username": "elsewhereuser"
        })))
        .mount(elsewhere.server())
        .await;

    h.cmd_for(&elsewhere.base_url())
        .args(["auth", "whoami", "--api-key", "throwaway-key"])
        .assert()
        .success()
        .stdout(predicate::str::contains("elsewhereuser"));
}

#[tokio::test]
async fn a_credential_from_before_binding_is_placed_by_its_profile() {
    let h = TestHarness::start().await;
    // Written by an older binary: no endpoint recorded on the credential, only
    // the profile that was in use at the time.
    h.write_config(json!({
        "default_profile": "default",
        "profiles": { "default": { "base_url": h.base_url() } }
    }));
    h.seed_secret(
        "default",
        json!({ "type": "api_key", "key": "legacy-key", "base_url": null }),
    );

    Mock::given(method("GET"))
        .and(path("/api/users/current"))
        .and(bearer_token("legacy-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 4,
            "username": "legacyuser"
        })))
        .mount(h.server())
        .await;

    // Still works against the environment it belongs to...
    h.cmd()
        .args(["auth", "whoami"])
        .assert()
        .success()
        .stdout(predicate::str::contains("legacyuser"));

    // ...and is still refused everywhere else.
    let elsewhere = wiremock::MockServer::start().await;
    h.cmd_for(&elsewhere.uri())
        .args(["auth", "whoami"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Refusing to send profile"));
}

#[tokio::test]
async fn logout_revokes_at_the_issuing_endpoint() {
    let h = TestHarness::start().await;
    h.seed_secret(
        "default",
        json!({
            "type": "oauth",
            "access_token": "access-token",
            "refresh_token": "refresh-token",
            "expires_at": "2099-01-01T00:00:00Z",
            "token_type": "Bearer",
            "scopes": ["wildcard:d"]
        }),
    );

    Mock::given(method("POST"))
        .and(path("/api/developers/oauth2/revoke"))
        .respond_with(ResponseTemplate::new(200))
        .mount(h.server())
        .await;

    // Logging out while pointed somewhere else must not hand the refresh token
    // to that endpoint — revocation is only meaningful at the issuer.
    let elsewhere = wiremock::MockServer::start().await;
    h.cmd_for(&elsewhere.uri())
        .args(["auth", "logout"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Logged out"));

    let stray = elsewhere
        .received_requests()
        .await
        .expect("no requests recorded");
    assert!(
        stray.is_empty(),
        "the refresh token was sent to an endpoint that never issued it: {:?}",
        stray.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );

    let revocations = h
        .server()
        .received_requests()
        .await
        .expect("no requests recorded")
        .iter()
        .filter(|r| r.url.path() == "/api/developers/oauth2/revoke")
        .count();
    assert_eq!(revocations, 1, "the issuer was not asked to revoke");

    // Local removal happens either way.
    h.cmd()
        .args(["auth", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not set"));
}

#[tokio::test]
async fn an_event_send_to_another_endpoint_leaves_the_profile_token_at_home() {
    let h = TestHarness::start().await;
    h.cmd()
        .args(["auth", "login", "--api-key", "production-key"])
        .assert()
        .success();

    // Event ingest authenticates with its integration key, so pointing one at
    // another environment is legitimate — it just must not carry this profile's
    // credential along with it.
    let elsewhere = TestHarness::start().await;
    Mock::given(method("POST"))
        .and(path("/api/events"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "alertKey": "abc123" })))
        .mount(elsewhere.server())
        .await;

    h.cmd_for(&elsewhere.base_url())
        .args(["event", "send", "-k", "int-key", "-s", "hi"])
        .assert()
        .success();

    let sent = elsewhere
        .server()
        .received_requests()
        .await
        .expect("no requests recorded");
    let carried_credentials = sent.iter().any(|r| r.headers.contains_key("authorization"));
    assert!(
        !carried_credentials,
        "an event send to another environment carried the profile's credential"
    );
}

#[tokio::test]
async fn an_inherited_api_key_stays_with_the_environment_that_exported_it() {
    let h = TestHarness::start().await;
    let elsewhere = TestHarness::start().await;

    // The CI shape: a key and an endpoint exported together, no stored
    // credential at all. Whoever exported the key never saw this command line.
    h.cmd_for(&elsewhere.base_url())
        .env("ILERT_API_KEY", "production-key")
        .env("ILERT_BASE_URL", h.base_url())
        .args(["auth", "whoami"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Refusing to send the API key from ILERT_API_KEY",
        ))
        .stderr(predicate::str::contains("--api-key"));

    let leaked = elsewhere
        .server()
        .received_requests()
        .await
        .expect("no requests recorded");
    assert!(
        leaked.is_empty(),
        "an exported key was offered to an unrelated endpoint: {:?}",
        leaked.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn an_inherited_api_key_works_where_its_environment_points() {
    let h = TestHarness::start().await;
    Mock::given(method("GET"))
        .and(path("/api/users/current"))
        .and(bearer_token("ci-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 7,
            "username": "ciuser"
        })))
        .mount(h.server())
        .await;

    h.cmd()
        .env("ILERT_API_KEY", "ci-key")
        .env("ILERT_BASE_URL", h.base_url())
        .args(["auth", "whoami"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ciuser"));
}

#[tokio::test]
async fn a_token_endpoint_redirect_is_not_followed() {
    let h = TestHarness::start().await;
    let sink = TestHarness::start().await;

    // 307 preserves the method and the body, so following one would repeat the
    // authorization code — and later the refresh token — at the new location.
    Mock::given(method("POST"))
        .and(path("/api/developers/oauth2/token"))
        .respond_with(ResponseTemplate::new(307).insert_header(
            "location",
            format!("{}/api/developers/oauth2/token", sink.base_url()).as_str(),
        ))
        .mount(h.server())
        .await;

    h.cmd()
        .env("ILERT_OAUTH_TEST_CODE", "test-auth-code")
        .args(["auth", "login"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("redirect"));

    let followed = sink
        .server()
        .received_requests()
        .await
        .expect("no requests recorded");
    assert!(
        followed.is_empty(),
        "the token request was replayed at the redirect target: {:?}",
        followed.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn importing_one_variable_keeps_the_rest_of_the_profile() {
    let h = TestHarness::start().await;
    h.write_config(json!({
        "default_profile": "other-env",
        "profiles": {
            "other-env": {
                "base_url": h.base_url(),
                "oauth_client_id": "other-env-client-id",
                "team_context": "sre"
            }
        }
    }));

    // Only a key is exported. The endpoint and application this profile was set
    // up with must survive, or its credential would stay bound to one
    // environment while the profile quietly fell back to production.
    h.cmd()
        .env("ILERT_API_KEY", "rotated-key")
        .args(["config", "import"])
        .assert()
        .success();

    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(h.config_file()).expect("config written"))
            .expect("config is valid JSON");
    let profile = &written["profiles"]["other-env"];
    assert_eq!(profile["base_url"], json!(h.base_url()));
    assert_eq!(profile["oauth_client_id"], "other-env-client-id");
    assert_eq!(profile["team_context"], "sre");

    // And the imported key is bound to that endpoint, not to production.
    let stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(h.secret_file()).expect("secret written"))
            .expect("secret file is valid JSON");
    assert_eq!(stored["other-env"]["base_url"], json!(h.base_url()));
}

#[tokio::test]
async fn an_inherited_api_key_does_not_follow_a_profile_named_on_the_command_line() {
    let h = TestHarness::start().await;
    let elsewhere = TestHarness::start().await;

    // Two profiles: the one this shell is set up for, and another environment.
    h.write_config(json!({
        "default_profile": "default",
        "profiles": {
            "default": { "base_url": h.base_url() },
            "other-env": { "base_url": elsewhere.base_url() }
        }
    }));

    // No `--base-url` here: the endpoint comes from the profile, and `--profile`
    // is the command line choosing it. That must not also choose which
    // environment the exported key counts as belonging to.
    h.bare_cmd()
        .env("ILERT_API_KEY", "production-key")
        .args(["--profile", "other-env", "auth", "whoami"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Refusing to send the API key from ILERT_API_KEY",
        ));

    let leaked = elsewhere
        .server()
        .received_requests()
        .await
        .expect("no requests recorded");
    assert!(
        leaked.is_empty(),
        "an exported key followed --profile to another environment: {:?}",
        leaked.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn an_inherited_api_key_serves_the_profile_the_environment_selects() {
    let h = TestHarness::start().await;
    h.write_config(json!({
        "default_profile": "default",
        "profiles": { "other-env": { "base_url": h.base_url() } }
    }));
    Mock::given(method("GET"))
        .and(path("/api/users/current"))
        .and(bearer_token("ci-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9,
            "username": "envuser"
        })))
        .mount(h.server())
        .await;

    // ILERT_PROFILE selects the profile the same way the key was selected, so
    // the pair is consistent and the key travels.
    h.bare_cmd()
        .env("ILERT_API_KEY", "ci-key")
        .env("ILERT_PROFILE", "other-env")
        .args(["auth", "whoami"])
        .assert()
        .success()
        .stdout(predicate::str::contains("envuser"));
}

#[tokio::test]
async fn update_refuses_to_run_the_installer_without_consent() {
    let h = TestHarness::start().await;

    // A test run is not a terminal, so this is the CI/agent path: the command
    // replaces the binary that is running, and nothing here can answer a
    // prompt. Refusing early also means the installer is never fetched.
    h.cmd()
        .args(["update"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--yes"));
}

#[tokio::test]
async fn update_is_offered_alongside_the_other_static_commands() {
    let h = TestHarness::start().await;

    // The update notice tells people to run `ilert update`, so it has to be a
    // command the help agrees exists.
    h.cmd()
        .args(["--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("update"));
}
