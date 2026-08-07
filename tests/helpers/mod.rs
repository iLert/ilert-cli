// The e2e suite drives the CLI through two seams that exist only in debug
// builds: the file-backed secret store (`ILERT_SECRET_FILE`) and the injected
// OAuth code (`ILERT_OAUTH_TEST_CODE`). Under `--release` both are compiled out,
// so the tests would quietly reach for the developer's real keyring and open a
// real browser. Fail at compile time instead of at 2am.
#[cfg(not(debug_assertions))]
compile_error!(
    "the e2e suite must run in debug (`cargo test`); its keyring and OAuth test \
     seams are compiled out of release builds"
);

use std::path::PathBuf;

use assert_cmd::Command;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Real OpenAPI spec bundled as a test fixture.
const OPENAPI_SPEC: &str = include_str!("../fixtures/openapi.json");

/// A small spec covering shapes the real one does not have: a destructive
/// non-DELETE operation, required query and header parameters, a required
/// request body, and a header parameter with a reserved name.
pub const CLASSIFICATION_SPEC: &str = include_str!("../fixtures/classification.json");

/// Test harness that manages a wiremock server and isolated CLI environment.
pub struct TestHarness {
    server: MockServer,
    config_dir: TempDir,
    cache_dir: TempDir,
}

impl TestHarness {
    /// Start a new test environment with mock server serving the real OpenAPI spec.
    pub async fn start() -> Self {
        Self::start_with_spec(OPENAPI_SPEC).await
    }

    /// Same, but serving a different spec fixture — for shapes the real spec
    /// does not contain.
    pub async fn start_with_spec(spec_json: &str) -> Self {
        let server = MockServer::start().await;

        let spec: serde_json::Value =
            serde_json::from_str(spec_json).expect("fixture spec is invalid JSON");

        Mock::given(method("GET"))
            .and(path("/api-docs/openapi.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(spec))
            .mount(&server)
            .await;

        let config_dir = TempDir::new().expect("failed to create temp config dir");
        let cache_dir = TempDir::new().expect("failed to create temp cache dir");

        Self {
            server,
            config_dir,
            cache_dir,
        }
    }

    /// Create a Command pre-configured with --base-url pointing at the mock server
    /// and environment variables for isolated config/cache directories.
    pub fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("ilert").expect("binary not found");
        cmd.args(["--base-url", &self.server.uri()])
            .env("XDG_CONFIG_HOME", self.config_dir.path())
            .env("XDG_CACHE_HOME", self.cache_dir.path())
            // Use a file-backed secret store so tests stay isolated and never
            // touch (or prompt for) the real OS keyring.
            .env("ILERT_SECRET_FILE", self.secret_file())
            // Prevent colored output from messing with assertions
            .env("NO_COLOR", "1");
        cmd
    }

    /// Path to the isolated, file-backed secret store used during tests.
    pub fn secret_file(&self) -> PathBuf {
        self.config_dir.path().join("secrets.json")
    }

    /// Seed the secret store with a single credential for the given profile.
    pub fn seed_secret(&self, profile: &str, credential_json: serde_json::Value) {
        let map = serde_json::json!({ profile: credential_json });
        std::fs::write(
            self.secret_file(),
            serde_json::to_string_pretty(&map).expect("serialize secret map"),
        )
        .expect("write secret file");
    }

    /// Path to the profile config file the CLI reads.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.path().join("ilert").join("config.json")
    }

    /// Write `config.json` verbatim — including shapes the CLI would never
    /// write itself, so tests can pin how it treats them.
    pub fn write_config(&self, config_json: serde_json::Value) {
        let path = self.config_file();
        std::fs::create_dir_all(path.parent().expect("config path has a parent"))
            .expect("create config dir");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&config_json).expect("serialize config"),
        )
        .expect("write config file");
    }

    /// Get a reference to the mock server (for mounting additional mocks).
    pub fn server(&self) -> &MockServer {
        &self.server
    }

    /// The mock server's base URL, for commands that take their own endpoint.
    pub fn base_url(&self) -> String {
        self.server.uri()
    }

    /// Seed the OpenAPI spec cache by running a command that triggers spec fetching.
    /// This ensures subsequent commands have the dynamic command tree available.
    pub async fn seed_cache(&self) {
        self.cmd()
            .args(["ops", "list", "-o", "raw"])
            .output()
            .expect("failed to seed cache");
    }

    /// Verify the spec endpoint was called at least once.
    pub async fn verify_spec_fetched(&self) {
        let requests = self
            .server
            .received_requests()
            .await
            .expect("no requests recorded");

        let spec_requests = requests
            .iter()
            .filter(|r| r.url.path() == "/api-docs/openapi.json")
            .count();

        assert!(
            spec_requests > 0,
            "Expected spec to be fetched, but it was not"
        );
    }

    /// Verify nothing at all was requested — including the spec, which is a
    /// network request like any other and so is off limits to `--dry-run`.
    pub async fn verify_nothing_was_requested(&self) {
        let requests = self
            .server
            .received_requests()
            .await
            .expect("no requests recorded");

        let paths: Vec<&str> = requests.iter().map(|r| r.url.path()).collect();
        assert!(
            paths.is_empty(),
            "Expected no requests at all, but got: {paths:?}"
        );
    }
}
