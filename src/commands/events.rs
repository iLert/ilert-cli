use anyhow::Result;
use clap::{Arg, ArgAction, ArgMatches, Command};
use colored::Colorize;
use serde_json::json;

use crate::cli::RunContext;
use crate::http::HttpClient;

pub fn command() -> Command {
    Command::new("event")
        .about("Send events to ilert")
        .arg_required_else_help(true)
        .subcommand(
            Command::new("send")
                .about("Send an alert event")
                .after_help(
                    "Examples:\n  \
                    ilert event send -k il1int... -s \"Server down\"\n  \
                    ilert event send -k KEY -t RESOLVE -s \"Recovered\" --alert-key srv1\n  \
                    ilert event send -k KEY -s \"Deploy\" --custom env=prod --custom sha=abc123",
                )
                .arg(
                    Arg::new("integration-key")
                        .short('k')
                        .long("integration-key")
                        .required(true)
                        .value_name("KEY")
                        .help("Alert source integration key"),
                )
                .arg(
                    Arg::new("event-type")
                        .short('t')
                        .long("type")
                        .value_name("TYPE")
                        .default_value("ALERT")
                        .help("Event type: ALERT, ACCEPT, RESOLVE, COMMENT"),
                )
                .arg(
                    Arg::new("summary")
                        .short('s')
                        .long("summary")
                        .required(true)
                        .value_name("TEXT")
                        .help("Event summary"),
                )
                .arg(
                    Arg::new("details")
                        .short('d')
                        .long("details")
                        .value_name("TEXT")
                        .help("Event details"),
                )
                .arg(
                    Arg::new("alert-key")
                        .long("alert-key")
                        .value_name("KEY")
                        .help("Alert deduplication key"),
                )
                .arg(
                    Arg::new("priority")
                        .short('p')
                        .long("priority")
                        .value_name("PRIORITY")
                        .help("Priority: HIGH, LOW"),
                )
                .arg(
                    Arg::new("link")
                        .long("link")
                        .action(ArgAction::Append)
                        .value_name("URL")
                        .help("Attach link (repeatable)"),
                )
                .arg(
                    Arg::new("custom")
                        .long("custom")
                        .action(ArgAction::Append)
                        .value_name("KEY=VALUE")
                        .help("Custom detail (repeatable): --custom env=prod"),
                )
                .arg(
                    Arg::new("routing-key")
                        .long("routing-key")
                        .value_name("KEY")
                        .help("Routing key to override escalation policy"),
                ),
        )
}

pub async fn handle(matches: &ArgMatches, client: &HttpClient, ctx: &RunContext) -> Result<()> {
    match matches.subcommand() {
        Some(("send", sub)) => handle_send(sub, client, ctx).await,
        _ => {
            eprintln!("Usage: ilert event send [OPTIONS]");
            Ok(())
        }
    }
}

async fn handle_send(matches: &ArgMatches, client: &HttpClient, ctx: &RunContext) -> Result<()> {
    let integration_key = matches
        .get_one::<String>("integration-key")
        .expect("required");
    let event_type = matches
        .get_one::<String>("event-type")
        .expect("has default");
    let summary = matches.get_one::<String>("summary").expect("required");

    let mut body = json!({
        "integrationKey": integration_key,
        "eventType": event_type,
        "summary": summary,
    });

    if let Some(details) = matches.get_one::<String>("details") {
        body["details"] = json!(details);
    }
    if let Some(alert_key) = matches.get_one::<String>("alert-key") {
        body["alertKey"] = json!(alert_key);
    }
    if let Some(priority) = matches.get_one::<String>("priority") {
        body["priority"] = json!(priority);
    }
    if let Some(routing_key) = matches.get_one::<String>("routing-key") {
        body["routingKey"] = json!(routing_key);
    }

    // Custom details
    if let Some(customs) = matches.get_many::<String>("custom") {
        let mut custom_details = serde_json::Map::new();
        for kv in customs {
            if let Some((k, v)) = kv.split_once('=') {
                custom_details.insert(k.to_string(), json!(v));
            }
        }
        if !custom_details.is_empty() {
            body["customDetails"] = serde_json::Value::Object(custom_details);
        }
    }

    // Links
    if let Some(links) = matches.get_many::<String>("link") {
        let link_objs: Vec<serde_json::Value> = links.map(|url| json!({"href": url})).collect();
        body["links"] = json!(link_objs);
    }

    let (_, response) = client
        .request(reqwest::Method::POST, "/api/events", &[], &[], Some(body))
        .await?;

    eprintln!("{} Event sent", "OK".green().bold());
    ctx.print_response(&response)
}
