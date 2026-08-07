use anyhow::Result;
use clap::{ArgMatches, Command};
use colored::Colorize;
use serde_json::Value;

use crate::cli::RunContext;
use crate::http::HttpClient;
use crate::output::OutputFormat;

pub fn command() -> Command {
    Command::new("status").about("Show system status overview")
}

pub async fn handle(_matches: &ArgMatches, client: &HttpClient, ctx: &RunContext) -> Result<()> {
    // The dashboard rendering is the table format; every other request for this
    // data — including `--jq` and `--fields` — wants the structured summary.
    if ctx.format != OutputFormat::Table || ctx.jq.is_some() || ctx.fields.is_some() {
        return handle_json(client, ctx).await;
    }

    // Fetch alerts count, active incidents, and services in parallel
    let alert_q: Vec<(String, String)> = vec![
        ("states".into(), "PENDING".into()),
        ("states".into(), "ACCEPTED".into()),
    ];
    let incident_q: Vec<(String, String)> = vec![
        ("states".into(), "INVESTIGATING".into()),
        ("states".into(), "IDENTIFIED".into()),
        ("states".into(), "MONITORING".into()),
    ];
    let (alerts_res, incidents_res, services_res) = tokio::join!(
        client.request(
            reqwest::Method::GET,
            "/api/alerts/count",
            &alert_q,
            &[],
            None
        ),
        client.request(
            reqwest::Method::GET,
            "/api/incidents",
            &incident_q,
            &[],
            None
        ),
        client.request(reqwest::Method::GET, "/api/services", &[], &[], None),
    );

    eprintln!();
    eprintln!("  {}", "ilert Status Overview".bold());
    eprintln!("  {}", "=".repeat(40).dimmed());

    // Alerts
    eprintln!();
    eprintln!("  {}", "Alerts".bold());
    match alerts_res {
        Ok((_, body)) => {
            let value = body.value();
            let count = value
                .get("count")
                .or(value.get("totalCount"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if count == 0 {
                eprintln!("  {} No open alerts", "OK".green().bold());
            } else {
                eprintln!("  {} {count} open alert(s)", "!!".red().bold());
            }
        }
        Err(e) => eprintln!("  {} Could not fetch alerts: {e}", "?".yellow()),
    }

    // Incidents
    eprintln!();
    eprintln!("  {}", "Incidents".bold());
    match incidents_res {
        Ok((_, body)) => {
            let value = body.value();
            let items = value.as_array().map(|a| a.len()).unwrap_or(0);
            if items == 0 {
                eprintln!("  {} No active incidents", "OK".green().bold());
            } else {
                eprintln!("  {} {items} active incident(s)", "!!".red().bold());
                if let Some(arr) = value.as_array() {
                    for inc in arr.iter().take(5) {
                        let summary = inc
                            .get("summary")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(no summary)");
                        let status = inc
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("UNKNOWN");
                        eprintln!(
                            "    {} {}",
                            colorize_incident_status(status),
                            summary.dimmed()
                        );
                    }
                }
            }
        }
        Err(e) => eprintln!("  {} Could not fetch incidents: {e}", "?".yellow()),
    }

    // Services
    eprintln!();
    eprintln!("  {}", "Services".bold());
    match services_res {
        Ok((_, body)) => {
            let value = body.value();
            let services = extract_items(value);
            if services.is_empty() {
                eprintln!("  {} No services configured", "--".dimmed());
            } else {
                let mut operational = 0u64;
                let mut degraded = 0u64;
                let mut outage = 0u64;

                for svc in &services {
                    let status = svc.get("status").and_then(|v| v.as_str()).unwrap_or("");
                    match status {
                        "OPERATIONAL" => operational += 1,
                        "DEGRADED" | "DEGRADED_PERFORMANCE" => degraded += 1,
                        _ if status.contains("OUTAGE") => outage += 1,
                        _ => {}
                    }
                }

                if outage > 0 {
                    eprintln!("  {} {outage} service(s) in outage", "!!".red().bold());
                }
                if degraded > 0 {
                    eprintln!("  {} {degraded} service(s) degraded", "!".yellow().bold());
                }
                if operational > 0 {
                    eprintln!(
                        "  {} {operational} service(s) operational",
                        "OK".green().bold()
                    );
                }

                // Show non-operational services by name
                for svc in &services {
                    let status = svc.get("status").and_then(|v| v.as_str()).unwrap_or("");
                    if status != "OPERATIONAL" {
                        let name = svc
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(unnamed)");
                        eprintln!("    {} {}", colorize_service_status(status), name);
                    }
                }
            }
        }
        Err(e) => eprintln!("  {} Could not fetch services: {e}", "?".yellow()),
    }

    eprintln!();
    Ok(())
}

async fn handle_json(client: &HttpClient, ctx: &RunContext) -> Result<()> {
    let alert_q: Vec<(String, String)> = vec![
        ("states".into(), "PENDING".into()),
        ("states".into(), "ACCEPTED".into()),
    ];
    let incident_q: Vec<(String, String)> = vec![
        ("states".into(), "INVESTIGATING".into()),
        ("states".into(), "IDENTIFIED".into()),
        ("states".into(), "MONITORING".into()),
    ];
    let (alerts_res, incidents_res, services_res) = tokio::join!(
        client.request(
            reqwest::Method::GET,
            "/api/alerts/count",
            &alert_q,
            &[],
            None
        ),
        client.request(
            reqwest::Method::GET,
            "/api/incidents",
            &incident_q,
            &[],
            None
        ),
        client.request(reqwest::Method::GET, "/api/services", &[], &[], None),
    );

    let summary = serde_json::json!({
        "alerts": alerts_res.map(|(_, b)| b.into_value()).unwrap_or(Value::Null),
        "incidents": incidents_res.map(|(_, b)| b.into_value()).unwrap_or(Value::Null),
        "services": services_res.map(|(_, b)| b.into_value()).unwrap_or(Value::Null),
    });

    ctx.print(&summary)
}

fn extract_items(value: &Value) -> Vec<&Value> {
    if let Some(arr) = value.as_array() {
        return arr.iter().collect();
    }
    if let Some(obj) = value.as_object() {
        for v in obj.values() {
            if let Some(arr) = v.as_array() {
                return arr.iter().collect();
            }
        }
    }
    Vec::new()
}

fn colorize_incident_status(status: &str) -> String {
    match status {
        "INVESTIGATING" => status.yellow().bold().to_string(),
        "IDENTIFIED" => status.cyan().bold().to_string(),
        "MONITORING" => status.blue().to_string(),
        "RESOLVED" => status.green().to_string(),
        _ => status.to_string(),
    }
}

fn colorize_service_status(status: &str) -> String {
    match status {
        "OPERATIONAL" => status.green().to_string(),
        "DEGRADED" | "DEGRADED_PERFORMANCE" => status.yellow().to_string(),
        s if s.contains("OUTAGE") => status.red().bold().to_string(),
        "UNDER_MAINTENANCE" => status.blue().to_string(),
        _ => status.to_string(),
    }
}
