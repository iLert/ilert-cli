use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use colored::Colorize;
use serde_json::json;

use crate::cli::RunContext;

const DEFAULT_BEAT_URL: &str = "https://beat.ilert.com/api/pings";

pub fn command() -> Command {
    Command::new("heartbeat")
        .about("Ping heartbeat monitors")
        .arg_required_else_help(true)
        .subcommand(
            Command::new("ping")
                .about("Send a heartbeat ping")
                .arg(
                    Arg::new("key")
                        .required(true)
                        .help("Heartbeat integration key"),
                )
                .arg(
                    Arg::new("beat-url")
                        .long("beat-url")
                        .value_name("URL")
                        .help("Custom beat URL, https:// (default: beat.ilert.com)"),
                ),
        )
}

pub async fn handle(matches: &ArgMatches, ctx: &RunContext) -> Result<()> {
    match matches.subcommand() {
        Some(("ping", sub)) => handle_ping(sub, ctx).await,
        _ => {
            eprintln!("Usage: ilert heartbeat ping <key>");
            Ok(())
        }
    }
}

async fn handle_ping(matches: &ArgMatches, ctx: &RunContext) -> Result<()> {
    let key = matches.get_one::<String>("key").expect("required");
    let beat_url = matches
        .get_one::<String>("beat-url")
        .map(String::as_str)
        .unwrap_or(DEFAULT_BEAT_URL);

    // The heartbeat key is the only credential this request carries, and it is
    // in the URL — so a cleartext `--beat-url` puts it on the wire in the clear
    // and into every proxy log on the way. The override stays available for
    // pointing at a local relay; it just has to be https, or loopback.
    if let Err(e) = crate::config::ensure_secure_base_url(beat_url) {
        return Err(crate::errors::CliError::user(format!("--beat-url: {e}")).into());
    }

    let url = format!(
        "{}/{}",
        beat_url.trim_end_matches('/'),
        crate::runner::path_segment("key", key)?
    );

    let client = crate::client::builder()
        .timeout(std::time::Duration::from_secs(10))
        // The key travels as a path segment, so a redirect would hand it to
        // whichever host the response names — and reqwest follows up to ten by
        // default. A ping has nowhere legitimate to be forwarded to.
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| transport_error(e, beat_url))?;
    let status = resp.status().as_u16();

    if (300..400).contains(&status) {
        return Err(crate::errors::CliError::user(format!(
            "The beat endpoint answered with a redirect (HTTP {status}), which this client will \
             not follow — the heartbeat key is part of the URL and would be handed to wherever \
             the redirect points. Check --beat-url."
        ))
        .into());
    }

    let body: serde_json::Value = resp.json().await.unwrap_or(json!({"status": status}));

    if status < 400 {
        eprintln!("{} Heartbeat ping sent", "OK".green().bold());
    } else {
        eprintln!(
            "{} Heartbeat ping failed (HTTP {status})",
            "Error:".red().bold()
        );
    }

    ctx.print(&body)
}

/// A failed ping, described without naming the URL it was sent to.
///
/// The heartbeat key *is* the last path segment, and reqwest's own message
/// quotes the full URL — "error sending request for url (https://…/<key>)" —
/// which would print the credential straight into whatever collects this CLI's
/// stderr: a CI log, a monitoring pipeline, a pasted bug report. `without_url`
/// drops it from the error and its sources; the host is the useful half and
/// carries no secret, so name that instead.
fn transport_error(err: reqwest::Error, beat_url: &str) -> anyhow::Error {
    let host = url::Url::parse(beat_url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| "the beat endpoint".to_string());
    crate::errors::CliError::user(format!(
        "Heartbeat ping to {host} failed: {}",
        err.without_url()
    ))
    .into()
}
