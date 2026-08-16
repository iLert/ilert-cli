use std::io::{self, BufRead, IsTerminal};

use anyhow::Result;
use clap::{Arg, ArgAction, ArgMatches, Command};
use colored::Colorize;

use crate::classification::Classification;
use crate::commands::{api, events, heartbeat, on_call, skills, status, version, watch};
use crate::config::{ConfigManager, Profile, ResolvedConfig};
use crate::errors::CliError;
use crate::http::HttpClient;
use crate::mode::CliMode;
use crate::oauth;
use crate::openapi::{self, Operation, OperationIndex, ParamLocation};
use crate::output::{self, OutputFormat};
use crate::preview::{self, OperationRef, RequestPreview};
use crate::runner::{self, BuildOptions, OperationRunner, RequestParams};
use crate::secret_store::{self, Credential};

/// Log verbosity level.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

impl LogLevel {
    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "debug" => Self::Debug,
            "warn" | "warning" => Self::Warn,
            "error" => Self::Error,
            _ => Self::Info,
        }
    }
}

/// Runtime context passed through dispatch.
pub struct RunContext {
    pub format: OutputFormat,
    pub fields: Option<Vec<String>>,
    pub quiet: bool,
    pub auto_confirm: bool,
    pub log_level: LogLevel,
    /// How this invocation was reached — a human, CI, or an agent. Decides
    /// whether a prompt is a question or a hang.
    pub mode: CliMode,
    /// Whether `-o` was actually on the command line. `format` alone cannot say:
    /// it falls back to JSON whenever stdout is not a terminal, so a piped run
    /// is indistinguishable from an explicit `-o json`.
    pub format_requested: bool,
    pub jq: Option<String>,
    /// `-H/--header`, applied to every request this run makes.
    pub headers: Vec<(String, String)>,
}

impl RunContext {
    /// Print JSON this process built itself — a summary, an envelope, a
    /// catalog. It is JSON by construction, so `--jq` always applies.
    pub fn print(&self, value: &serde_json::Value) -> Result<()> {
        if let Some(ref expression) = self.jq {
            print!("{}", crate::jq::filter(expression, value)?);
            return Ok(());
        }
        if let Some(ref fields) = self.fields {
            output::print_output_with_fields(value, self.format, Some(fields));
        } else {
            output::print_output(value, self.format);
        }
        Ok(())
    }

    /// Print a body that came off the wire.
    ///
    /// Identical to [`print`](Self::print) except that `--jq` is refused when
    /// the server did not send JSON. The check reads the flag the decoder set
    /// rather than the shape of the value, so a response whose body really is
    /// the JSON string `"ok"` filters like any other JSON.
    pub fn print_response(&self, body: &crate::http::ResponseBody) -> Result<()> {
        if self.jq.is_some() && !body.is_json() {
            return Err(CliError::user(
                "--jq needs a JSON response, but the server returned a non-JSON body. \
                 Re-run without --jq (or with '-o raw') to see it.",
            )
            .into());
        }
        self.print(body.value())
    }

    /// The `--dry-run` / refusal envelope, always as pretty JSON on stdout.
    ///
    /// It deliberately ignores `-o`, `--fields` and `--jq`: the envelope is a
    /// contract, and a caller that asked for a preview needs the same shape
    /// whatever else is on the command line.
    pub fn print_envelope(&self, value: &serde_json::Value) {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
    }

    /// Refuse `--jq` on a command whose output is not JSON at all — markdown,
    /// a shell script, a full-screen TUI. A filter that quietly does nothing is
    /// worse than an error, so no such command may simply ignore the flag.
    pub fn reject_jq(&self, what: &str, alternative: Option<&str>) -> Result<()> {
        if self.jq.is_some() {
            let mut msg = format!("--jq cannot filter {what}, which is not JSON. Drop --jq");
            match alternative {
                Some(hint) => msg.push_str(&format!(", or use {hint} instead.")),
                None => msg.push('.'),
            }
            return Err(CliError::user(msg).into());
        }
        Ok(())
    }

    /// Whether it is safe to ask a question. Both signals have to agree: an
    /// agent can hold a pty, and `ILERT_CLI_MODE=interactive` can be set on
    /// something that has no terminal at all.
    pub fn can_prompt(&self) -> bool {
        self.mode.can_prompt() && std::io::stdin().is_terminal()
    }

    pub fn info(&self, msg: &str) {
        if !self.quiet && self.log_level >= LogLevel::Info {
            eprintln!("{msg}");
        }
    }

    pub fn debug(&self, msg: &str) {
        if self.log_level >= LogLevel::Debug {
            eprintln!("{} {msg}", "debug:".dimmed());
        }
    }

    pub fn warn(&self, msg: &str) {
        if self.log_level >= LogLevel::Warn {
            eprintln!("{} {msg}", "warn:".yellow());
        }
    }
}

/// Names reserved for static subcommands — dynamic tags must not collide.
const STATIC_COMMANDS: &[&str] = &[
    "auth",
    "config",
    "completions",
    "ops",
    "api",
    "event",
    "heartbeat",
    "on-call",
    "skills",
    "status",
    "version",
    "dashboard",
    "help",
];

pub struct Cli {
    config_manager: ConfigManager,
    cached_index: Option<OperationIndex>,
}

impl Cli {
    pub async fn new() -> Result<Self> {
        let config_manager = ConfigManager::load()?;
        // The dynamic command tree is built from the cached spec, and which spec
        // that is depends on the environment — so the base URL has to be known
        // before the parser that would normally report it can be constructed.
        let base_url = bootstrap_base_url(&config_manager);
        let cached_index = openapi::load_cached_index(&base_url)?;
        Ok(Self {
            config_manager,
            cached_index,
        })
    }

    pub async fn run(mut self) -> Result<()> {
        let cmd = build_command(&self.cached_index);
        let matches = match cmd.try_get_matches() {
            Ok(m) => m,
            Err(e) => e.exit(),
        };

        // Handle --no-color
        if matches.get_flag("no-color") {
            colored::control::set_override(false);
        }

        // Handle --quiet: redirect stderr to sink
        let quiet = matches.get_flag("quiet");

        let format_flag = matches.get_one::<String>("output").map(String::as_str);
        let output_format = OutputFormat::from_flag(format_flag);

        let fields: Option<Vec<String>> = matches
            .get_one::<String>("fields")
            .map(|f| f.split(',').map(|s| s.trim().to_string()).collect());

        let auto_confirm = matches.get_flag("yes");

        let detection = crate::mode::detect_full()?;
        let jq = matches.get_one::<String>("jq").cloned();
        let headers = api::parse_global_headers(&matches)?;

        let log_level = matches
            .get_one::<String>("log-level")
            .map(|s| LogLevel::from_str(s))
            .unwrap_or(if quiet {
                LogLevel::Error
            } else {
                LogLevel::Info
            });

        let resolved = self.config_manager.resolve(
            matches.get_one::<String>("profile").map(String::as_str),
            matches.get_one::<String>("api-key").map(String::as_str),
            matches.get_one::<String>("base-url").map(String::as_str),
            matches
                .get_one::<String>("oauth-client-id")
                .map(String::as_str),
            matches
                .get_one::<String>("team-context")
                .map(String::as_str),
        );

        // Background update check (fire and forget, skip if quiet)
        if !quiet {
            tokio::spawn(version::check_for_updates());
        }

        let ctx = RunContext {
            format: output_format,
            fields,
            quiet,
            auto_confirm,
            log_level,
            mode: detection.mode,
            format_requested: format_flag.is_some(),
            jq,
            headers,
        };

        ctx.debug(&format!(
            "mode={} (from {})",
            detection.mode.as_str(),
            detection.source
        ));

        let result = self.dispatch(&matches, &resolved, &ctx).await;

        if let Err(ref err) = result {
            output::print_error(err, output_format);
            std::process::exit(crate::errors::exit_code(err));
        }

        Ok(())
    }

    async fn dispatch(
        &mut self,
        matches: &ArgMatches,
        config: &ResolvedConfig,
        ctx: &RunContext,
    ) -> Result<()> {
        match matches.subcommand() {
            Some(("auth", sub)) => self.handle_auth(sub, config, ctx).await,
            Some(("config", sub)) => self.handle_config(sub, config, ctx),
            Some(("completions", sub)) => self.handle_completions(sub, ctx),
            Some(("ops", sub)) => self.handle_ops(sub, config, ctx).await,
            Some(("api", sub)) => self.handle_api(sub, config, ctx).await,
            Some(("event", sub)) => {
                let client = self.make_client(config, false, ctx).await?;
                events::handle(sub, &client, ctx).await
            }
            Some(("heartbeat", sub)) => heartbeat::handle(sub, ctx).await,
            Some(("on-call", sub)) => {
                let client = self.make_client(config, true, ctx).await?;
                on_call::handle(sub, &client, ctx).await
            }
            Some(("status", sub)) => {
                let client = self.make_client(config, true, ctx).await?;
                status::handle(sub, &client, ctx).await
            }
            Some(("skills", sub)) => match skills::handle(sub)? {
                skills::SkillsOutput::Structured(value) => ctx.print(&value),
                skills::SkillsOutput::Markdown(text) => {
                    ctx.reject_jq("a skill document", Some("'ilert skills list -o json'"))?;
                    print!("{text}");
                    Ok(())
                }
            },
            Some(("version", _)) => version::handle(ctx),
            Some(("dashboard", _)) => {
                ctx.reject_jq("the dashboard", Some("'ilert status -o json'"))?;
                let client = self.make_client(config, true, ctx).await?;
                crate::tui::run_dashboard(&client).await
            }
            Some((tag, sub)) => self.handle_dynamic(tag, sub, config, ctx).await,
            None => {
                build_command(&self.cached_index).print_help()?;
                Ok(())
            }
        }
    }

    // -- Helpers --

    async fn make_client(
        &self,
        config: &ResolvedConfig,
        require_auth: bool,
        ctx: &RunContext,
    ) -> Result<HttpClient> {
        let api_key = if require_auth {
            Some(config.resolve_credential().await?)
        } else {
            config.resolve_credential_opt().await?
        };
        Ok(HttpClient::new(
            config.base_url.clone(),
            api_key,
            config.team_context.clone(),
        )
        .with_debug(ctx.log_level >= LogLevel::Debug)
        .with_extra_headers(ctx.headers.clone()))
    }

    /// The request as the caller will see it in a preview or refusal, including
    /// the global `-H` headers the client will attach.
    fn preview_of(&self, ctx: &RunContext, req: &RequestParams) -> RequestPreview {
        let mut request = req.preview();
        let mut headers = ctx.headers.clone();
        headers.extend(request.headers);
        request.headers = headers;
        request
    }

    /// Gate a destructive command.
    ///
    /// A human at a terminal gets a prompt. Anything else gets the same JSON
    /// envelope as `--dry-run` on stderr and exit status 2 — because a prompt
    /// nobody can answer is a hang, and silently proceeding is worse.
    fn confirm(
        &self,
        ctx: &RunContext,
        label: &str,
        classification: Classification,
        operation: &OperationRef,
        request: &RequestPreview,
    ) -> Result<()> {
        if !classification.destructive || ctx.auto_confirm {
            return Ok(());
        }

        if ctx.can_prompt() {
            let confirmed = dialoguer::Confirm::new()
                .with_prompt(format!("{label}: this cannot be undone. Continue?"))
                .default(false)
                .interact()?;
            return if confirmed {
                Ok(())
            } else {
                Err(CliError::Cancelled.into())
            };
        }

        Err(
            CliError::confirmation_required(preview::refusal(operation, classification, request))
                .into(),
        )
    }

    // -- Auth commands --

    async fn handle_auth(
        &mut self,
        matches: &ArgMatches,
        config: &ResolvedConfig,
        ctx: &RunContext,
    ) -> Result<()> {
        match matches.subcommand() {
            Some(("login", sub)) => {
                let base_url = sub
                    .get_one::<String>("base-url")
                    .map(String::as_str)
                    .unwrap_or(&config.base_url)
                    .to_string();
                // Already resolved through flag → env → profile → default, so a
                // profile that was set up against another environment keeps its
                // application on a re-login without repeating the flag.
                let client_id = config.oauth_client_id.clone();

                // Choose the auth path: --api-key / --with-token => API key,
                // otherwise the interactive OAuth browser flow.
                let (mut cred, method_label) =
                    if let Some(key) = sub.get_one::<String>("api-key").cloned() {
                        (
                            Credential::ApiKey {
                                key,
                                base_url: None,
                            },
                            "API key",
                        )
                    } else if sub.get_flag("with-token") {
                        (
                            Credential::ApiKey {
                                key: read_token_from_stdin()?,
                                base_url: None,
                            },
                            "API key",
                        )
                    } else {
                        let oauth_config = oauth::OauthConfig {
                            base_url: &base_url,
                            client_id: &client_id,
                        };
                        (oauth::run_login_flow(oauth_config, ctx).await?, "OAuth")
                    };

                // Bind the credential to the environment it was obtained from.
                // `auth login` is the one place a `--base-url` override is
                // meant to change environments; from here on, that binding is
                // what every other command is checked against.
                cred.bind_to(&crate::config::normalize_base_url(&base_url));
                secret_store::store(&config.profile_name, &cred)?;

                // Persist non-secret profile settings; the credential itself
                // just went to the keyring.
                let profile = Profile {
                    base_url: Some(base_url.clone()),
                    // Only persisted when it is not the compiled-in production
                    // id, so a production profile keeps tracking the binary
                    // across a rotation instead of pinning today's value.
                    oauth_client_id: (client_id != oauth::DEFAULT_CLIENT_ID).then_some(client_id),
                    team_context: sub.get_one::<String>("team-context").cloned(),
                };

                self.config_manager
                    .save_profile(&config.profile_name, profile)?;
                self.config_manager
                    .set_default_profile(&config.profile_name)?;

                ctx.info(&format!(
                    "{} Logged in to profile '{}' via {}",
                    "OK".green().bold(),
                    config.profile_name,
                    method_label
                ));
                ctx.info("Fetching API spec...");
                match openapi::ensure_spec(&base_url).await {
                    Ok(index) => {
                        self.cached_index = Some(index);
                        ctx.info(&format!(
                            "{} API spec cached. Run 'ilert --help' to see all available commands.",
                            "OK".green().bold()
                        ));
                    }
                    Err(e) => {
                        ctx.info(&format!(
                            "{} Could not fetch API spec: {e}",
                            "Warning:".yellow().bold()
                        ));
                    }
                }
                Ok(())
            }
            Some(("logout", _)) => {
                // Best-effort revoke of an OAuth refresh token before deleting.
                if let Ok(Some(cred)) = secret_store::retrieve(&config.profile_name)
                    && let Some(rt) = cred.refresh_token()
                {
                    // Revoke at the endpoint that issued the token, never at
                    // whatever `--base-url` this invocation happens to carry:
                    // revocation only means anything at the issuer, and sending
                    // the token anywhere else would disclose it on the way out.
                    let issuer = config.credential_endpoint(&cred);
                    if issuer != crate::config::normalize_base_url(&config.base_url) {
                        ctx.warn(&format!(
                            "revoking at {issuer}, where this credential was issued — \
                             not at the endpoint given on the command line"
                        ));
                    }
                    oauth::revoke(&issuer, rt).await;
                }
                secret_store::delete(&config.profile_name)?;
                ctx.info(&format!(
                    "{} Logged out of profile '{}'",
                    "OK".green().bold(),
                    config.profile_name
                ));
                Ok(())
            }
            Some(("whoami", _)) => {
                let client = self.make_client(config, true, ctx).await?;
                let (_, user) = client
                    .request(reqwest::Method::GET, "/api/users/current", &[], &[], None)
                    .await?;
                ctx.print_response(&user)
            }
            Some(("show", _)) => {
                // Build a flat object so it renders cleanly in table output
                // (nested objects collapse to "{...}" in the table view).
                let mut info = serde_json::Map::new();
                info.insert("profile".into(), serde_json::json!(config.profile_name));
                info.insert("base_url".into(), serde_json::json!(config.base_url));
                info.insert(
                    "oauth_client_id".into(),
                    serde_json::json!(config.oauth_client_id),
                );
                info.insert(
                    "team_context".into(),
                    serde_json::json!(config.team_context),
                );

                let stored = secret_store::retrieve(&config.profile_name)?;
                // The environment the stored credential may be sent to, which
                // is the thing to check first when a command is refused.
                if let Some(ref cred) = stored {
                    info.insert(
                        "credential_endpoint".into(),
                        serde_json::json!(config.credential_endpoint(cred)),
                    );
                }

                match stored {
                    Some(Credential::ApiKey { key, .. }) => {
                        info.insert("auth_type".into(), serde_json::json!("api_key"));
                        info.insert("api_key".into(), serde_json::json!(mask_api_key(&key)));
                    }
                    Some(Credential::OAuth {
                        access_token,
                        refresh_token,
                        expires_at,
                        token_type,
                        scopes,
                        ..
                    }) => {
                        info.insert("auth_type".into(), serde_json::json!("oauth"));
                        info.insert(
                            "access_token".into(),
                            serde_json::json!(mask_api_key(&access_token)),
                        );
                        info.insert("token_type".into(), serde_json::json!(token_type));
                        info.insert(
                            "expires_at".into(),
                            serde_json::json!(expires_at.to_rfc3339()),
                        );
                        info.insert(
                            "refresh_token".into(),
                            serde_json::json!(if refresh_token.is_some() {
                                "present"
                            } else {
                                "none"
                            }),
                        );
                        info.insert("scopes".into(), serde_json::json!(scopes.join(" ")));
                    }
                    None => {
                        // Nothing stored — an explicit (flag/env) key may still
                        // be carrying this invocation.
                        match config.explicit_api_key.as_ref() {
                            Some(k) => {
                                info.insert("auth_type".into(), serde_json::json!("api_key"));
                                info.insert(
                                    "api_key".into(),
                                    serde_json::json!(mask_api_key(&k.key)),
                                );
                                // An exported key is pinned to its environment;
                                // one passed on the command line is not.
                                info.insert(
                                    "credential_endpoint".into(),
                                    match k.bound_to.as_deref() {
                                        Some(endpoint) => serde_json::json!(endpoint),
                                        None => serde_json::json!("(given on the command line)"),
                                    },
                                );
                            }
                            None => {
                                info.insert("auth_type".into(), serde_json::json!("none"));
                                info.insert("api_key".into(), serde_json::json!("(not set)"));
                            }
                        }
                    }
                }
                ctx.print(&serde_json::Value::Object(info))
            }
            _ => {
                eprintln!("Usage: ilert auth <login|logout|whoami|show>");
                Ok(())
            }
        }
    }

    // -- Config commands --

    fn handle_config(
        &mut self,
        matches: &ArgMatches,
        config: &ResolvedConfig,
        ctx: &RunContext,
    ) -> Result<()> {
        match matches.subcommand() {
            Some(("list", _)) => {
                let profiles = self.config_manager.list_profiles();
                let items: Vec<serde_json::Value> = profiles
                    .iter()
                    .map(|(name, is_default)| {
                        serde_json::json!({ "name": name, "default": is_default })
                    })
                    .collect();
                ctx.print(&serde_json::Value::Array(items))
            }
            Some(("show", _)) => {
                let info = serde_json::json!({
                    "profile": config.profile_name,
                    "base_url": config.base_url,
                    "oauth_client_id": config.oauth_client_id,
                    "team_context": config.team_context,
                    "config_path": ConfigManager::config_file_path()?.to_string_lossy(),
                    "cache_dir": ConfigManager::cache_dir()?.to_string_lossy(),
                });
                ctx.print(&info)
            }
            Some(("import", _)) => {
                self.handle_config_import(config)?;
                Ok(())
            }
            _ => {
                eprintln!("Usage: ilert config <list|show|import>");
                Ok(())
            }
        }
    }

    fn handle_config_import(&mut self, config: &ResolvedConfig) -> Result<()> {
        let api_key = std::env::var("ILERT_API_KEY").ok();
        let base_url = std::env::var("ILERT_BASE_URL").ok();
        let oauth_client_id = std::env::var("ILERT_OAUTH_CLIENT_ID").ok();
        let team_context = std::env::var("ILERT_TEAM_CONTEXT").ok();

        if api_key.is_none()
            && base_url.is_none()
            && oauth_client_id.is_none()
            && team_context.is_none()
        {
            return Err(crate::errors::CliError::user(
                "No ILERT_* environment variables found to import.",
            )
            .into());
        }

        let name = std::env::var("ILERT_PROFILE").unwrap_or_else(|_| config.profile_name.clone());

        // Import is additive. Exporting only `ILERT_API_KEY` and importing it
        // into an existing profile must not blank out the endpoint and OAuth
        // application that profile was set up with — that would leave its
        // credential bound to one environment while the profile silently fell
        // back to production, and nothing would work again until someone
        // noticed why.
        let existing = self
            .config_manager
            .profile(&name)
            .cloned()
            .unwrap_or_default();
        let profile = Profile {
            base_url: base_url.or(existing.base_url),
            oauth_client_id: oauth_client_id.or(existing.oauth_client_id),
            team_context: team_context.or(existing.team_context),
        };

        // Store the key in the keyring (not plaintext); keep other settings in
        // config. The key is bound to the endpoint it belongs to — the imported
        // one, else the profile's own, else production.
        if let Some(key) = api_key {
            let endpoint = crate::config::normalize_base_url(
                profile.base_url.as_deref().unwrap_or(&config.base_url),
            );
            secret_store::store(
                &name,
                &Credential::ApiKey {
                    key,
                    base_url: Some(endpoint),
                },
            )?;
        }

        self.config_manager.save_profile(&name, profile)?;
        self.config_manager.set_default_profile(&name)?;

        eprintln!(
            "{} Imported environment variables into profile '{name}'",
            "OK".green().bold()
        );
        Ok(())
    }

    // -- Completions --

    fn handle_completions(&self, matches: &ArgMatches, ctx: &RunContext) -> Result<()> {
        ctx.reject_jq("a shell completion script", None)?;
        let shell = matches.get_one::<String>("shell").expect("required");
        let shell: clap_complete::Shell = shell
            .parse()
            .map_err(|_| crate::errors::CliError::user(format!("Unknown shell: {shell}")))?;
        let mut cmd = build_command(&self.cached_index);
        clap_complete::generate(shell, &mut cmd, "ilert", &mut std::io::stdout());
        Ok(())
    }

    // -- Ops --

    async fn handle_ops(
        &self,
        matches: &ArgMatches,
        config: &ResolvedConfig,
        ctx: &RunContext,
    ) -> Result<()> {
        if let Some(("run", sub)) = matches.subcommand() {
            let op_id = sub
                .get_one::<String>("operation-id")
                .expect("required")
                .clone();
            return self.run_operation(&op_id, sub, config, ctx).await;
        }

        let index = openapi::ensure_spec(&config.base_url).await?;
        match matches.subcommand() {
            Some(("list", sub)) => {
                let tag_filter = sub.get_one::<String>("tag").map(String::as_str);
                let mut ops: Vec<serde_json::Value> = Vec::new();
                for (tag, operations) in &index.by_tag {
                    if tag_filter.is_some_and(|f| f != tag) {
                        continue;
                    }
                    for op in operations {
                        ops.push(serde_json::json!({
                            "id": op.id, "tag": op.tag, "action": op.action,
                            "method": op.method, "path": op.path, "summary": op.summary,
                            "classification": op.classification.to_json(),
                        }));
                    }
                }
                ops.sort_by(|a, b| {
                    let ta = a["tag"].as_str().unwrap_or("");
                    let tb = b["tag"].as_str().unwrap_or("");
                    ta.cmp(tb).then(
                        a["action"]
                            .as_str()
                            .unwrap_or("")
                            .cmp(b["action"].as_str().unwrap_or("")),
                    )
                });
                ctx.print(&serde_json::Value::Array(ops))
            }
            Some(("show", sub)) => {
                let id = sub.get_one::<String>("operation-id").expect("required");
                let op = index.by_id.get(id.as_str()).ok_or_else(|| {
                    crate::errors::CliError::user(format!("Unknown operation: {id}"))
                })?;
                let info = serde_json::json!({
                    "id": op.id, "method": op.method, "path": op.path,
                    "tag": op.tag, "action": op.action,
                    "summary": op.summary, "description": op.description,
                    "classification": op.classification.to_json(),
                    "parameters": op.parameters.iter().map(|p| serde_json::json!({
                        "name": p.name, "in": format!("{:?}", p.location).to_lowercase(),
                        "required": p.required, "description": p.description,
                    })).collect::<Vec<_>>(),
                    "requestBodySchema": op.request_body_schema,
                });
                ctx.print(&info)
            }
            _ => {
                eprintln!("Usage: ilert ops <list|show|run>");
                Ok(())
            }
        }
    }

    /// `ilert ops run <operation-id>` — execute a spec operation by ID.
    async fn run_operation(
        &self,
        op_id: &str,
        matches: &ArgMatches,
        config: &ResolvedConfig,
        ctx: &RunContext,
    ) -> Result<()> {
        let dry_run = matches.get_flag("dry-run");

        let index = self.require_index(config, !dry_run).await?;
        let operation = index
            .by_id
            .get(op_id)
            .ok_or_else(|| CliError::user(format!("Unknown operation: {op_id}")))?;

        // Built before anything reaches the keyring or the network: a dry run
        // and a refusal both stop after this point.
        let req = runner::build_params(
            operation,
            matches,
            &BuildOptions {
                base_url: &config.base_url,
                allow_prompting: ctx.can_prompt() && !dry_run,
                templated_path: false,
            },
        )?;
        let request = self.preview_of(ctx, &req);
        let op_ref = runner::operation_ref(operation);

        if dry_run {
            ctx.print_envelope(&preview::dry_run(
                &op_ref,
                operation.classification,
                &request,
            ));
            return Ok(());
        }

        self.confirm(
            ctx,
            &format!("{} {}", operation.tag, operation.action),
            operation.classification,
            &op_ref,
            &request,
        )?;

        let client = self.make_client(config, true, ctx).await?;
        let (_, body) = OperationRunner::new(&client).send(operation, &req).await?;
        ctx.print_response(&body)
    }

    // -- Raw path passthrough --

    async fn handle_api(
        &self,
        matches: &ArgMatches,
        config: &ResolvedConfig,
        ctx: &RunContext,
    ) -> Result<()> {
        let target = matches
            .get_one::<String>("target")
            .expect("required")
            .clone();

        // Compatibility window: `ilert api <operation-id>` used to execute an
        // operation. Keep it working for one release, loudly.
        if !target.starts_with('/') {
            ctx.warn(&format!(
                "'ilert api {target}' now expects a path starting with '/'. \
                 Treating '{target}' as an operation ID for now — \
                 use 'ilert ops run {target}' instead."
            ));
            return self.run_operation(&target, matches, config, ctx).await;
        }

        let dry_run = matches.get_flag("dry-run");
        let req = api::build_request(matches, &config.base_url)?;

        let classification = Classification::from_method(req.method.as_str());
        let op_ref = OperationRef::new(format!("api {} {}", req.method, req.path));
        let request = RequestPreview {
            method: req.method.to_string(),
            url: req.url.clone(),
            query: req.query.clone(),
            headers: ctx.headers.clone(),
            body: req.body.clone(),
        };

        if dry_run {
            ctx.print_envelope(&preview::dry_run(&op_ref, classification, &request));
            return Ok(());
        }

        self.confirm(
            ctx,
            &format!("{} {}", req.method, req.path),
            classification,
            &op_ref,
            &request,
        )?;

        let client = self
            .make_client(config, true, ctx)
            .await?
            .with_verbose(matches.get_flag("verbose"));

        let include = matches.get_flag("include");

        // `--include` asks for the response metadata, and a 404's status and
        // headers are exactly when that matters most. So the error path prints
        // the same three things as the success path and only then fails.
        let response = client
            .request_raw(
                req.method.clone(),
                &req.path,
                &req.query,
                &req.headers,
                req.body.clone(),
            )
            .await?;

        if include {
            api::print_response_meta(response.status, &response.headers);
        }

        if response.status >= 400 {
            if include {
                ctx.print_response(&response.body)?;
            }
            return Err(crate::http::http_error(&response));
        }

        ctx.print_response(&response.body)
    }

    // -- Dynamic OpenAPI commands --

    async fn handle_dynamic(
        &self,
        tag: &str,
        matches: &ArgMatches,
        config: &ResolvedConfig,
        ctx: &RunContext,
    ) -> Result<()> {
        // Without a cached spec this tag arrived as an external subcommand, so
        // its arguments are still raw strings — which is the only place a
        // `--dry-run` can be seen before deciding whether to fetch the spec.
        let index = self
            .require_index(config, !raw_args_contain(matches, "--dry-run"))
            .await?;

        if !index.by_tag.contains_key(tag) {
            let suggestion = suggest_similar(tag, index.by_tag.keys().map(String::as_str));
            let msg = if let Some(s) = suggestion {
                format!("Unknown resource: '{tag}'. Did you mean '{s}'?")
            } else {
                format!("Unknown resource: '{tag}'. Run 'ilert --help' to see available resources.")
            };
            return Err(crate::errors::CliError::user(msg).into());
        }

        let (action, sub_matches) = matches.subcommand().ok_or_else(|| {
            let actions = index.actions_for_tag(tag);
            crate::errors::CliError::user(format!("Usage: ilert {tag} <{}>", actions.join("|")))
        })?;

        // Handle convenience aliases
        if tag == "alerts" && (action == "ack" || action == "resolve" || action == "assign") {
            return self
                .handle_alert_alias(action, sub_matches, config, ctx)
                .await;
        }

        let operation = index.find_by_tag_action(tag, action).ok_or_else(|| {
            let available = index.actions_for_tag(tag);
            let suggestion = suggest_similar(action, available.iter().copied());
            let msg = if let Some(s) = suggestion {
                format!("Unknown action '{action}' for '{tag}'. Did you mean '{s}'?")
            } else {
                format!(
                    "Unknown action '{action}' for '{tag}'. Available: {}",
                    available.join(", ")
                )
            };
            crate::errors::CliError::user(msg)
        })?;

        let dry_run = sub_matches.get_flag("dry-run");
        let paginate_all = sub_matches
            .try_get_one::<bool>("all")
            .ok()
            .flatten()
            .copied()
            .unwrap_or(false);
        let watch_interval = sub_matches.try_get_one::<String>("watch").ok().flatten();
        let pipe_stdin = sub_matches
            .try_get_one::<bool>("stdin")
            .ok()
            .flatten()
            .copied()
            .unwrap_or(false);

        // The request is built exactly once, before any credential is resolved
        // and before stdin is read. `--dry-run` and a refusal both stop right
        // after it, so neither touches the keyring or the network — and a
        // refusal can describe precisely what it refused without re-running an
        // interactive prompt or consuming the pipe it was about to read.
        let req = runner::build_params(
            operation,
            sub_matches,
            &BuildOptions {
                base_url: &config.base_url,
                allow_prompting: ctx.can_prompt() && !dry_run && !pipe_stdin,
                templated_path: pipe_stdin,
            },
        )?;
        let request = self.preview_of(ctx, &req);
        let op_ref = runner::operation_ref(operation);

        if dry_run {
            ctx.print_envelope(&preview::dry_run(
                &op_ref,
                operation.classification,
                &request,
            ));
            return Ok(());
        }

        // Every path below this line sends something, so every path below this
        // line is gated — including `--stdin`, where one answer stands for the
        // whole batch, and `--watch`, which repeats forever.
        self.confirm(
            ctx,
            &format!("{tag} {action}"),
            operation.classification,
            &op_ref,
            &request,
        )?;

        let client = self.make_client(config, true, ctx).await?;
        let runner = OperationRunner::new(&client);

        if let Some(interval_str) = watch_interval {
            let interval: u64 = interval_str.parse().unwrap_or(5);
            return watch::run_watch(
                &client,
                operation,
                &req.path,
                &req.query,
                &req.headers,
                Some(interval),
                ctx,
            )
            .await;
        }

        if pipe_stdin {
            return self.handle_pipe(operation, &req, &client, ctx).await;
        }

        if paginate_all {
            let value = runner
                .execute_paginated(operation, &req, sub_matches)
                .await?;
            return ctx.print(&value);
        }

        let (_, body) = runner.send(operation, &req).await?;
        ctx.print_response(&body)
    }

    async fn handle_alert_alias(
        &self,
        action: &str,
        matches: &ArgMatches,
        config: &ResolvedConfig,
        ctx: &RunContext,
    ) -> Result<()> {
        let id = matches.get_one::<String>("id").expect("required");

        let (path, body, verb) = match action {
            "assign" => {
                let user = matches
                    .get_one::<String>("user")
                    .ok_or_else(|| CliError::user("--user is required for assign"))?;
                (
                    format!("/api/alerts/{id}/assign"),
                    Some(serde_json::json!({ "username": user })),
                    format!("assigned to {user}"),
                )
            }
            "ack" => (
                format!("/api/alerts/{id}/accept"),
                None,
                "accepted".to_string(),
            ),
            _ => (
                format!("/api/alerts/{id}/resolve"),
                None,
                "resolved".to_string(),
            ),
        };

        // These aliases never reach the OpenAPI index, so their classification
        // comes from the static table instead of an HTTP method.
        let label = format!("alerts {action}");
        let classification = crate::classification::for_static_command(&label)
            .unwrap_or_else(|| Classification::from_method("PUT"));
        let op_ref = OperationRef::new(label.clone());
        let request = RequestPreview {
            method: "PUT".into(),
            url: format!("{}{}", config.base_url, path),
            query: Vec::new(),
            headers: ctx.headers.clone(),
            body: body.clone(),
        };

        // Same order as every other command that can send: preview first, then
        // the confirmation gate, and only then a credential.
        if matches.get_flag("dry-run") {
            ctx.print_envelope(&preview::dry_run(&op_ref, classification, &request));
            return Ok(());
        }

        self.confirm(ctx, &label, classification, &op_ref, &request)?;

        let client = self.make_client(config, true, ctx).await?;
        let (_, body) = client
            .request(reqwest::Method::PUT, &path, &[], &[], body)
            .await?;

        ctx.info(&format!("{} Alert {id} {verb}", "OK".green().bold()));
        ctx.print_response(&body)
    }

    /// One request per line of stdin, against the template the caller already
    /// confirmed. `req` carries the query, headers and body resolved from the
    /// command line, so `--stdin` varies only the ID.
    async fn handle_pipe(
        &self,
        operation: &Operation,
        req: &RequestParams,
        client: &HttpClient,
        ctx: &RunContext,
    ) -> Result<()> {
        let stdin = io::stdin();
        if stdin.is_terminal() {
            return Err(crate::errors::CliError::user(
                "--stdin requires piped input, e.g.: echo 42 | ilert alerts get --stdin",
            )
            .into());
        }

        let method: reqwest::Method = operation
            .method
            .parse()
            .map_err(|_| CliError::user(format!("Invalid HTTP method: {}", operation.method)))?;

        let mut results = Vec::new();
        for line in stdin.lock().lines() {
            let id = line?.trim().to_string();
            if id.is_empty() {
                continue;
            }

            // Every line picks its own path, so each one has to be proven to be
            // a single segment before it is substituted — the preview the
            // caller confirmed described the template, not this value.
            let segment = match crate::runner::path_segment("id", &id) {
                Ok(segment) => segment,
                Err(e) => {
                    eprintln!("{} {e}", "Error:".red().bold());
                    continue;
                }
            };

            let path = req
                .path
                .replace("{id}", &segment)
                .replace("{user-id}", &segment);

            match client
                .request(
                    method.clone(),
                    &path,
                    &req.query,
                    &req.headers,
                    req.body.clone(),
                )
                .await
            {
                Ok((_, body)) => results.push(body.into_value()),
                Err(e) => {
                    eprintln!("{} {id}: {e}", "Error:".red().bold());
                }
            }
        }

        ctx.print(&serde_json::Value::Array(results))
    }

    /// The operation index, fetching the spec only if allowed to.
    ///
    /// Fetching the spec is a network request like any other, so `--dry-run`
    /// must not do it. With no cached spec there is also nothing to preview
    /// against, so the honest answer is to say what is missing rather than to
    /// quietly reach out for it.
    async fn require_index(
        &self,
        config: &ResolvedConfig,
        allow_fetch: bool,
    ) -> Result<OperationIndex> {
        if let Some(ref index) = self.cached_index {
            return Ok(index.clone());
        }
        if !allow_fetch {
            return Err(CliError::user(
                "--dry-run cannot preview this command: the API spec is not cached yet, and \
                 downloading it would be a network request. Run 'ilert ops list' once to \
                 cache the spec, then try again.",
            )
            .into());
        }
        openapi::ensure_spec(&config.base_url).await
    }
}

// ---------------------------------------------------------------------------
// Command tree builder
// ---------------------------------------------------------------------------

/// The base URL as far as it can be known before the command line is parsed.
///
/// [`Cli::new`] needs it to choose a spec cache, but the parser that reads
/// `--profile` and `--base-url` cannot be built until that cache has been read.
/// Those two flags are therefore scanned straight off `argv`; everything else
/// — `ILERT_BASE_URL`, `ILERT_PROFILE`, the stored profile, the default — comes
/// from the normal resolution chain.
///
/// A misread costs a spec fetch under a different cache key and nothing else:
/// [`Cli::run`] re-resolves the real configuration from the parsed matches, so
/// no request is ever sent on the strength of this.
fn bootstrap_base_url(config_manager: &ConfigManager) -> String {
    let args: Vec<String> = std::env::args().collect();
    config_manager
        .resolve(
            scan_flag(&args, "--profile").as_deref(),
            None,
            scan_flag(&args, "--base-url").as_deref(),
            None,
            None,
        )
        .base_url
}

/// The value of a long option on the raw command line, in either
/// `--flag value` or `--flag=value` form.
fn scan_flag(args: &[String], flag: &str) -> Option<String> {
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let Some(tail) = arg.strip_prefix(flag) else {
            continue;
        };
        if tail.is_empty() {
            return rest.next().cloned();
        }
        if let Some(value) = tail.strip_prefix('=') {
            return Some(value.to_string());
        }
    }
    None
}

fn build_command(index: &Option<OperationIndex>) -> Command {
    let mut app = Command::new("ilert")
        .about(
            "The official ilert CLI\n\n\
             AI agents: run `ilert skills show ilert-essentials` before your first command.",
        )
        .version(version::current_version())
        .arg_required_else_help(true)
        .after_help(
            "Examples:\n  \
            ilert auth login                         Authenticate (browser/OAuth)\n  \
            ilert alerts list                        List open alerts\n  \
            ilert alerts ack 42                      Accept alert #42\n  \
            ilert alerts list --all -o json          Export all alerts as JSON\n  \
            ilert incidents create --set summary=... Create an incident\n  \
            ilert event send -k <KEY> -s \"msg\"       Send an alert event\n  \
            ilert status                             Show system overview\n  \
            ilert dashboard                          Open live TUI dashboard\n  \
            ilert skills show ilert-essentials       Rules and gotchas not covered by --help",
        )
        .arg(
            Arg::new("output")
                .short('o')
                .long("output")
                .value_name("FORMAT")
                .help("Output format: table, json, ndjson, raw")
                .global(true),
        )
        .arg(
            Arg::new("profile")
                .long("profile")
                .value_name("NAME")
                .help("Config profile to use")
                .global(true),
        )
        .arg(
            Arg::new("api-key")
                .long("api-key")
                .value_name("KEY")
                .help("API key (overrides profile)")
                .global(true),
        )
        .arg(
            Arg::new("base-url")
                .long("base-url")
                .value_name("URL")
                .help("API base URL (overrides profile)")
                .global(true),
        )
        .arg(
            Arg::new("oauth-client-id")
                .long("oauth-client-id")
                .value_name("ID")
                .help("OAuth2 client ID (overrides profile; non-production environments)")
                .global(true),
        )
        .arg(
            Arg::new("team-context")
                .long("team-context")
                .value_name("ID")
                .help("Team context ID (overrides profile)")
                .global(true),
        )
        .arg(
            Arg::new("quiet")
                .short('q')
                .long("quiet")
                .action(ArgAction::SetTrue)
                .help("Suppress informational output on stderr")
                .global(true),
        )
        .arg(
            Arg::new("no-color")
                .long("no-color")
                .action(ArgAction::SetTrue)
                .help("Disable colored output")
                .global(true),
        )
        .arg(
            Arg::new("fields")
                .long("fields")
                .value_name("FIELDS")
                .help("Comma-separated list of columns for table output")
                .global(true),
        )
        .arg(
            Arg::new("yes")
                .short('y')
                .long("yes")
                .action(ArgAction::SetTrue)
                .help("Skip confirmation prompts")
                .global(true),
        )
        .arg(
            Arg::new("log-level")
                .long("log-level")
                .value_name("LEVEL")
                .help("Log verbosity: error, warn, info, debug")
                .global(true),
        )
        .arg(
            Arg::new("jq")
                .long("jq")
                .value_name("EXPR")
                .help("Filter JSON output through a jq expression")
                .global(true),
        )
        .arg(
            Arg::new("header")
                .short('H')
                .long("header")
                .value_name("KEY: VALUE")
                .action(ArgAction::Append)
                .help("Add a custom header to every request")
                .global(true),
        )
        // Static subcommands
        .subcommand(build_auth_command())
        .subcommand(build_config_command())
        .subcommand(build_completions_command())
        .subcommand(build_ops_command())
        .subcommand(api::command())
        .subcommand(events::command())
        .subcommand(heartbeat::command())
        .subcommand(on_call::command())
        .subcommand(skills::command())
        .subcommand(status::command())
        .subcommand(Command::new("version").about("Show version information"))
        .subcommand(Command::new("dashboard").about("Open interactive TUI dashboard"));

    if let Some(index) = index {
        app = attach_dynamic_commands(app, index);
    } else {
        app = app.allow_external_subcommands(true);
    }

    app
}

fn attach_dynamic_commands(mut app: Command, index: &OperationIndex) -> Command {
    let mut tags: Vec<&String> = index.by_tag.keys().collect();
    tags.sort();

    for tag in tags {
        if STATIC_COMMANDS.contains(&tag.as_str()) {
            continue;
        }

        let operations = &index.by_tag[tag];
        let mut tag_cmd = Command::new(tag.clone())
            .about(format!("Manage {tag} resources"))
            .arg_required_else_help(true);

        for op in operations {
            let action_cmd = build_operation_command(op);
            tag_cmd = tag_cmd.subcommand(action_cmd);
        }

        // Add convenience aliases for alerts
        if tag == "alerts" {
            tag_cmd = tag_cmd
                .subcommand(build_alias_command("ack", "Accept an alert by ID", "id"))
                .subcommand(build_alias_command(
                    "resolve",
                    "Resolve an alert by ID",
                    "id",
                ))
                .subcommand(
                    Command::new("assign")
                        .about("Assign an alert to a user")
                        .arg(Arg::new("id").required(true).help("Alert ID"))
                        .arg(
                            Arg::new("user")
                                .long("user")
                                .required(true)
                                .value_name("USERNAME")
                                .help("Username to assign to"),
                        )
                        .arg(dry_run_arg()),
                );
        }

        app = app.subcommand(tag_cmd);
    }

    app
}

fn build_operation_command(op: &Operation) -> Command {
    let fallback = format!("{} {}", op.method, op.path);
    let about = op.summary.as_deref().unwrap_or(&fallback);

    let mut cmd = Command::new(op.action.clone())
        .about(about.to_string())
        .long_about(op.description.clone().unwrap_or_default());

    // `--stdin` supplies the ID for every request it makes, so demanding `--id`
    // as well would be asking for a value that is about to be ignored.
    let takes_stdin = op
        .parameters
        .iter()
        .any(|p| p.name == "id" && p.location == ParamLocation::Path);

    // The IDs `--stdin` substitutes into the path. Passing one explicitly as
    // well is never what the caller meant: the piped value wins for `{id}` and
    // `{user-id}`, so `--id 7` alongside a pipe would silently either be
    // overwritten or make every line target 7.
    let stdin_substitutes = |name: &str| matches!(name, "id" | "user-id");

    for param in &op.parameters {
        let mut arg = Arg::new(param.name.clone())
            .long(param.name.clone())
            .value_name(param_value_name(param));
        if let Some(ref desc) = param.description {
            arg = arg.help(desc.clone());
        }
        match param.location {
            ParamLocation::Path if takes_stdin && stdin_substitutes(&param.name) => {
                arg = arg.required_unless_present("stdin").conflicts_with("stdin");
            }
            ParamLocation::Path => arg = arg.required(true),
            ParamLocation::Query => arg = arg.required(param.required),
            ParamLocation::Header => arg = arg.required(param.required),
        }
        cmd = cmd.arg(arg);
    }

    if op.has_request_body {
        cmd = cmd
            .arg(
                Arg::new("body")
                    .long("body")
                    .help("Request body JSON (use '-' for stdin)"),
            )
            .arg(
                Arg::new("body-file")
                    .long("body-file")
                    .help("Path to JSON file for request body"),
            )
            .arg(
                Arg::new("set")
                    .long("set")
                    .action(ArgAction::Append)
                    .help("Set body field: --set key=value"),
            );
    }

    if op.action == "list" || (op.method == "GET" && !op.path.ends_with('}')) {
        cmd = cmd
            .arg(
                Arg::new("all")
                    .long("all")
                    .action(ArgAction::SetTrue)
                    .help("Fetch all pages automatically"),
            )
            .arg(
                Arg::new("watch")
                    .long("watch")
                    .value_name("SECS")
                    .help("Re-fetch every N seconds (default: 5)"),
            );
    }

    // Pipe support for get/delete/update operations that take an ID
    if takes_stdin {
        cmd = cmd.arg(
            Arg::new("stdin")
                .long("stdin")
                .action(ArgAction::SetTrue)
                .help("Read IDs from stdin (one per line)"),
        );
    }

    cmd = cmd.arg(dry_run_arg());

    cmd
}

/// Every command in the hand-written tree, spelled as it is dispatched
/// (`"auth login"`, `"on-call now"`).
///
/// The dynamic tree is excluded on purpose: those commands carry a
/// classification resolved from the spec. This is the list that has to be
/// classified by hand, and `classification::every_static_command_is_classified`
/// reads it so that adding a command cannot quietly skip the confirmation gate.
#[cfg(test)]
pub fn static_command_paths() -> Vec<String> {
    fn walk(cmd: &Command, prefix: &str, out: &mut Vec<String>) {
        let name = cmd.get_name();
        if name == "help" {
            return;
        }
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix} {name}")
        };

        let children: Vec<&Command> = cmd
            .get_subcommands()
            .filter(|s| s.get_name() != "help")
            .collect();

        // A parent that refuses to run without a subcommand is not itself a
        // command anyone can invoke, so it needs no classification. `on-call`
        // is the opposite case: it runs bare and also has `now`.
        if children.is_empty() || !cmd.is_arg_required_else_help_set() {
            out.push(path.clone());
        }
        for child in children {
            walk(child, &path, out);
        }
    }

    let root = build_command(&None);
    let mut out = Vec::new();
    for sub in root.get_subcommands() {
        walk(sub, "", &mut out);
    }
    out
}

/// Whether an unparsed external subcommand was given `flag`.
///
/// clap hands the arguments of an external subcommand over verbatim, under the
/// empty argument id, because it has no definition to match them against.
fn raw_args_contain(matches: &ArgMatches, flag: &str) -> bool {
    matches
        .try_get_many::<std::ffi::OsString>("")
        .ok()
        .flatten()
        .is_some_and(|mut args| args.any(|arg| arg == flag))
}

fn build_alias_command(name: &'static str, about: &'static str, id_param: &'static str) -> Command {
    Command::new(name)
        .about(about)
        .arg(Arg::new(id_param).required(true).help("Alert ID"))
        .arg(dry_run_arg())
}

/// The preview flag, identical on every command that can send a request.
fn dry_run_arg() -> Arg {
    Arg::new("dry-run")
        .long("dry-run")
        .action(ArgAction::SetTrue)
        .help("Preview the request without sending")
}

fn param_value_name(param: &crate::openapi::Parameter) -> String {
    if let Some(ref schema) = param.schema
        && let Some(typ) = schema.get("type").and_then(|v| v.as_str())
    {
        return match typ {
            "integer" | "number" => "NUMBER",
            "boolean" => "BOOL",
            "array" => "VALUES",
            _ => "VALUE",
        }
        .to_string();
    }
    param.name.to_uppercase()
}

// ---------------------------------------------------------------------------
// Static subcommand builders
// ---------------------------------------------------------------------------

fn build_auth_command() -> Command {
    Command::new("auth")
        .about("Manage authentication")
        .arg_required_else_help(true)
        .after_help(
            "Examples:\n  \
            ilert auth login                          Log in via your browser (OAuth)\n  \
            ilert auth login --api-key il1api...      Log in with an API key\n  \
            echo $KEY | ilert auth login --with-token  Headless API-key login (stdin)\n  \
            ilert auth whoami                          Check who you are\n  \
            ilert auth show                            Show current auth (secrets masked)\n\n\
            No browser available (e.g. over SSH)? Use --with-token or set ILERT_API_KEY.\n\n\
            Another ilert environment? Give it its own profile — the endpoint and the\n\
            OAuth application that goes with it are stored together:\n  \
            ilert --profile <name> auth login --base-url <url> --oauth-client-id <id>",
        )
        .subcommand(
            Command::new("login")
                .about("Authenticate with ilert (OAuth by default)")
                .arg(
                    Arg::new("api-key")
                        .long("api-key")
                        .help("Log in with an API key instead of OAuth"),
                )
                .arg(
                    Arg::new("with-token")
                        .long("with-token")
                        .action(ArgAction::SetTrue)
                        .help("Read an API key from stdin (headless/CI)"),
                )
                .arg(Arg::new("base-url").long("base-url").help("API base URL"))
                .arg(
                    Arg::new("team-context")
                        .long("team-context")
                        .help("Default team context"),
                ),
        )
        .subcommand(Command::new("logout").about("Remove stored credentials"))
        .subcommand(Command::new("whoami").about("Show current user info"))
        .subcommand(Command::new("show").about("Show current auth config"))
}

/// Read an API key/token from stdin, trimming whitespace.
fn read_token_from_stdin() -> Result<String> {
    use std::io::Read;
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf).map_err(|e| {
        crate::errors::CliError::user(format!("Failed to read token from stdin: {e}"))
    })?;
    let token = buf.trim().to_string();
    if token.is_empty() {
        return Err(crate::errors::CliError::user("No token provided on stdin").into());
    }
    Ok(token)
}

fn build_config_command() -> Command {
    Command::new("config")
        .about("Manage CLI configuration")
        .arg_required_else_help(true)
        .subcommand(Command::new("list").about("List all profiles"))
        .subcommand(Command::new("show").about("Show current configuration"))
        .subcommand(
            Command::new("import").about("Import config from ILERT_* environment variables"),
        )
}

fn build_completions_command() -> Command {
    Command::new("completions")
        .about("Generate shell completions")
        .arg(
            Arg::new("shell")
                .required(true)
                .help("Shell: bash, zsh, fish, powershell"),
        )
}

fn build_ops_command() -> Command {
    Command::new("ops")
        .about("Discover API operations")
        .arg_required_else_help(true)
        .subcommand(
            Command::new("list")
                .about("List all operations")
                .arg(Arg::new("tag").long("tag").help("Filter by resource tag")),
        )
        .subcommand(
            Command::new("show").about("Show operation details").arg(
                Arg::new("operation-id")
                    .required(true)
                    .help("Operation ID to inspect"),
            ),
        )
        .subcommand(
            Command::new("run")
                .about("Execute an operation by ID")
                .after_help(
                    "Examples:\n  \
                     ilert ops run getAlert --param id=42\n  \
                     ilert ops run createAlertSource --set name=Prometheus --dry-run\n\n\
                     For an endpoint the cached spec does not know about, use 'ilert api <path>'.",
                )
                .arg(
                    Arg::new("operation-id")
                        .required(true)
                        .help("Operation ID to execute"),
                )
                .arg(
                    Arg::new("param")
                        .long("param")
                        .value_name("NAME=VALUE")
                        .action(ArgAction::Append)
                        .help("Set a path, query or header parameter"),
                )
                .arg(
                    Arg::new("body")
                        .long("body")
                        .help("Request body JSON (use '-' for stdin)"),
                )
                .arg(
                    Arg::new("body-file")
                        .long("body-file")
                        .help("Path to JSON file for request body"),
                )
                .arg(
                    Arg::new("set")
                        .long("set")
                        .action(ArgAction::Append)
                        .help("Set body field: --set key=value"),
                )
                .arg(
                    Arg::new("dry-run")
                        .long("dry-run")
                        .action(ArgAction::SetTrue)
                        .help("Preview the request without sending"),
                ),
        )
}

fn mask_api_key(key: &str) -> String {
    if key.len() <= 8 {
        return "****".to_string();
    }
    format!("{}...{}", &key[..4], &key[key.len() - 4..])
}

/// Find the most similar string from candidates using Levenshtein distance.
/// Returns None if no candidate is within a reasonable edit distance.
fn suggest_similar<'a>(input: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
    let input_lower = input.to_lowercase();
    let max_distance = (input.len() / 3).max(2); // allow ~1 typo per 3 chars, min 2

    candidates
        .map(|c| (c, levenshtein(&input_lower, &c.to_lowercase())))
        .filter(|(_, dist)| *dist <= max_distance)
        .min_by_key(|(_, dist)| *dist)
        .map(|(c, _)| c.to_string())
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());

    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}
