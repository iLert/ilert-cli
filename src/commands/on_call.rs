use anyhow::Result;
use clap::{ArgMatches, Command};
use colored::Colorize;

use crate::cli::RunContext;
use crate::http::HttpClient;
use crate::output::{self, OutputFormat};
use crate::sanitize::{terminal_string, terminal_text};

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
    // Without `expand` the API returns bare `{"id": …}` references for the
    // user, policy and schedule, which is nothing anyone can read — the summary
    // would be a column of "Unknown". The parameter repeats per entity.
    let query: Vec<(String, String)> = ["user", "escalationPolicy", "schedule"]
        .iter()
        .map(|entity| ("expand".to_string(), (*entity).to_string()))
        .collect();

    let (_, body) = client
        .request(reqwest::Method::GET, "/api/on-calls", &query, &[], None)
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
        eprintln!("{}", on_call_line(item));
    }
    eprintln!();
}

/// One rendered "who is on call" line.
///
/// Policy names, schedule names and user names are all account content — an
/// escalation policy called `"\u{1b}]52;c;…"` is a name the API will store and
/// hand back — so each is escaped before it is coloured, and our own colours go
/// on afterwards.
fn on_call_line(item: &serde_json::Value) -> String {
    let policy = terminal_text(
        item.get("escalationPolicy")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("Unknown policy"),
    );

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
    let user = terminal_string(user);

    let schedule = terminal_text(
        item.get("schedule")
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or(""),
    );

    format!(
        "  {} {}{}",
        user.green().bold(),
        format!("({policy})").dimmed(),
        if schedule.is_empty() {
            String::new()
        } else {
            format!(" {}", format!("via {schedule}").dimmed())
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_field_of_an_on_call_entry_can_reach_the_terminal_raw() {
        let _colors = crate::testutil::colors(false);
        let hostile = json!({
            "user": {
                "firstName": "\u{1b}[2JAda",
                "lastName": "Lovelace\u{202E}",
            },
            "escalationPolicy": { "name": "\u{1b}]52;c;cm0gLXJmIC8=\u{7}" },
            "schedule": { "name": "nights\nOK: nobody paged" },
        });

        let line = on_call_line(&hostile);
        assert!(!line.contains('\u{1b}'), "{line:?}");
        assert!(!line.contains('\u{7}'), "{line:?}");
        assert!(!line.contains('\n'), "{line:?}");
        assert!(!line.contains('\u{202E}'), "{line:?}");
        // The text itself is still legible, just inert.
        assert!(line.contains("Ada"), "{line:?}");
    }

    #[test]
    fn an_ordinary_entry_reads_normally() {
        let entry = json!({
            "user": { "firstName": "Ada", "lastName": "Lovelace" },
            "escalationPolicy": { "name": "Störung Eskalation" },
            "schedule": { "name": "Nachtdienst" },
        });

        let _colors = crate::testutil::colors(false);
        let line = on_call_line(&entry);
        assert_eq!(line, "  Ada Lovelace (Störung Eskalation) via Nachtdienst");
    }

    #[test]
    fn a_missing_user_falls_back_without_inventing_one() {
        let line = on_call_line(&json!({ "escalationPolicy": { "name": "P1" } }));
        assert!(line.contains("Unknown"), "{line:?}");
    }
}
