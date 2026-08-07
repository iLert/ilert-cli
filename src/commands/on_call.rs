use anyhow::Result;
use clap::{ArgMatches, Command};
use colored::Colorize;

use crate::cli::RunContext;
use crate::http::HttpClient;
use crate::output::{self, OutputFormat};

pub fn command() -> Command {
    Command::new("on-call")
        .about("Show who is currently on call")
        .subcommand(Command::new("now").about("Show current on-call schedules"))
}

pub async fn handle(matches: &ArgMatches, client: &HttpClient, ctx: &RunContext) -> Result<()> {
    match matches.subcommand() {
        Some(("now", _)) | None => handle_now(client, ctx).await,
        _ => {
            eprintln!("Usage: ilert on-call now");
            Ok(())
        }
    }
}

async fn handle_now(client: &HttpClient, ctx: &RunContext) -> Result<()> {
    let (_, body) = client
        .request(reqwest::Method::GET, "/api/on-calls", &[], &[], None)
        .await?;

    // The prose summary is the table rendering. Anything that asked for data —
    // another format, `--fields`, `--jq` — goes through the shared output path.
    if ctx.format == OutputFormat::Table && ctx.jq.is_none() && ctx.fields.is_none() {
        print_on_call_summary(body.value());
        return Ok(());
    }

    ctx.print_response(&body)
}

fn print_on_call_summary(value: &serde_json::Value) {
    let items = value.as_array().or_else(|| {
        value.as_object().and_then(|obj| {
            for v in obj.values() {
                if let Some(arr) = v.as_array() {
                    return Some(arr);
                }
            }
            None
        })
    });

    let items = match items {
        Some(arr) => arr,
        None => {
            output::print_output(value, OutputFormat::Table);
            return;
        }
    };

    if items.is_empty() {
        eprintln!("{}", "No one is currently on call.".dimmed());
        return;
    }

    eprintln!("{}", "  Currently on call:".bold());
    eprintln!();

    for item in items {
        let policy = item
            .get("escalationPolicy")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("Unknown policy");

        let user = item
            .get("user")
            .and_then(|u| {
                let first = u.get("firstName").and_then(|v| v.as_str()).unwrap_or("");
                let last = u.get("lastName").and_then(|v| v.as_str()).unwrap_or("");
                let username = u.get("username").and_then(|v| v.as_str()).unwrap_or("");
                if !first.is_empty() || !last.is_empty() {
                    Some(format!("{first} {last}").trim().to_string())
                } else if !username.is_empty() {
                    Some(username.to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "Unknown".to_string());

        let schedule = item
            .get("schedule")
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");

        eprintln!(
            "  {} {} {}",
            user.green().bold(),
            format!("({policy})").dimmed(),
            if schedule.is_empty() {
                String::new()
            } else {
                format!("via {schedule}").dimmed().to_string()
            }
        );
    }
    eprintln!();
}
