use anyhow::Result;
use clap::{ArgMatches, Command};
use colored::Colorize;
use serde_json::Value;

use crate::cli::RunContext;
use crate::http::HttpClient;
use crate::output::OutputFormat;
use crate::sanitize::{terminal_string, terminal_text};

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
        // An error can carry a message the gateway wrote, so it is no safer to
        // print than the data it stands in for.
        Err(e) => eprintln!(
            "  {} Could not fetch alerts: {}",
            "?".yellow(),
            terminal_string(e.to_string())
        ),
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
                            terminal_text(summary).dimmed()
                        );
                    }
                }
            }
        }
        Err(e) => eprintln!(
            "  {} Could not fetch incidents: {}",
            "?".yellow(),
            terminal_string(e.to_string())
        ),
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
                        eprintln!(
                            "    {} {}",
                            colorize_service_status(status),
                            terminal_text(name)
                        );
                    }
                }
            }
        }
        Err(e) => eprintln!(
            "  {} Could not fetch services: {}",
            "?".yellow(),
            terminal_string(e.to_string())
        ),
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

/// Both colorizers escape first and match afterwards. The recognised states are
/// plain ASCII, so escaping cannot stop one from matching — but the `_` arm
/// prints whatever the API sent, and that is the arm an attacker aims for.
fn colorize_incident_status(status: &str) -> String {
    let status = &terminal_text(status);
    match status.as_str() {
        "INVESTIGATING" => status.yellow().bold().to_string(),
        "IDENTIFIED" => status.cyan().bold().to_string(),
        "MONITORING" => status.blue().to_string(),
        "RESOLVED" => status.green().to_string(),
        _ => status.to_string(),
    }
}

fn colorize_service_status(status: &str) -> String {
    let status = &terminal_text(status);
    match status.as_str() {
        "OPERATIONAL" => status.green().to_string(),
        "DEGRADED" | "DEGRADED_PERFORMANCE" => status.yellow().to_string(),
        s if s.contains("OUTAGE") => status.red().bold().to_string(),
        "UNDER_MAINTENANCE" => status.blue().to_string(),
        _ => status.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dashboard prints a state string straight through when it does not
    /// recognise it, which is exactly the case a hostile value arranges for.
    #[test]
    fn an_unrecognised_state_cannot_write_escapes_to_the_terminal() {
        // Colour off so any escape left in the output came from the payload —
        // "OK\rMAJOR_OUTAGE" still matches the outage arm and would otherwise
        // arrive wrapped in our own codes.
        let _colors = crate::testutil::colors(false);
        for state in [
            "\u{1b}[2JOPERATIONAL",
            "\u{1b}]52;c;cm0gLXJmIC8=\u{7}",
            "OK\rMAJOR_OUTAGE",
            "\u{9b}31mDEGRADED",
            "\u{202E}LANOITAREPO",
        ] {
            for rendered in [
                colorize_service_status(state),
                colorize_incident_status(state),
            ] {
                assert!(!rendered.contains('\u{1b}'), "{state:?} -> {rendered:?}");
                assert!(!rendered.contains('\u{7}'), "{state:?} -> {rendered:?}");
                assert!(!rendered.contains('\r'), "{state:?} -> {rendered:?}");
                assert!(!rendered.contains('\u{9b}'), "{state:?} -> {rendered:?}");
                assert!(!rendered.contains('\u{202E}'), "{state:?} -> {rendered:?}");
            }
        }
    }

    /// Escaping runs before the match, so the states we do know must still be
    /// recognised — and coloured with our own codes.
    #[test]
    fn the_known_states_still_match_and_still_colour() {
        let _colors = crate::testutil::colors(true);
        let operational = colorize_service_status("OPERATIONAL");
        let investigating = colorize_incident_status("INVESTIGATING");
        let outage = colorize_service_status("MAJOR_OUTAGE");

        for (rendered, text) in [
            (&operational, "OPERATIONAL"),
            (&investigating, "INVESTIGATING"),
            (&outage, "MAJOR_OUTAGE"),
        ] {
            assert!(rendered.contains(text), "{rendered:?}");
            assert!(rendered.contains('\u{1b}'), "not coloured: {rendered:?}");
        }
    }
}
