use std::io::{self, IsTerminal, Write};

use colored::Colorize;
use serde_json::Value;
use tabled::builder::Builder;
use tabled::settings::Style;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutputFormat {
    Table,
    Json,
    Ndjson,
    Raw,
}

impl OutputFormat {
    pub fn from_flag(flag: Option<&str>) -> Self {
        match flag {
            Some("table") => Self::Table,
            Some("json") => Self::Json,
            Some("ndjson") => Self::Ndjson,
            Some("raw") => Self::Raw,
            _ => {
                if io::stdout().is_terminal() {
                    Self::Table
                } else {
                    Self::Json
                }
            }
        }
    }
}

const PREFERRED_COLUMNS: &[&str] = &[
    "id",
    "name",
    "summary",
    "status",
    "state",
    "type",
    "priority",
    "createdAt",
    "updatedAt",
];

const MAX_COLUMNS: usize = 8;
const MAX_CELL_WIDTH: usize = 48;

pub fn print_output(value: &Value, format: OutputFormat) {
    print_output_with_fields(value, format, None);
}

pub fn print_output_with_fields(value: &Value, format: OutputFormat, fields: Option<&[String]>) {
    match format {
        OutputFormat::Table => print_table(value, fields),
        OutputFormat::Json => print_json(value),
        OutputFormat::Ndjson => print_ndjson(value),
        OutputFormat::Raw => print_raw(value),
    }
}

fn print_json(value: &Value) {
    let out = serde_json::to_string_pretty(value).unwrap_or_default();
    println!("{out}");
}

fn print_ndjson(value: &Value) {
    if let Some(items) = extract_items(value) {
        for item in items {
            println!("{}", serde_json::to_string(item).unwrap_or_default());
        }
    } else {
        println!("{}", serde_json::to_string(value).unwrap_or_default());
    }
}

fn print_raw(value: &Value) {
    print!("{}", serde_json::to_string(value).unwrap_or_default());
    let _ = io::stdout().flush();
}

fn print_table(value: &Value, fields: Option<&[String]>) {
    let items = match extract_items(value) {
        Some(items) if !items.is_empty() => items,
        _ => {
            if let Some(obj) = value.as_object() {
                print_single_object(obj);
            } else {
                println!("{value}");
            }
            return;
        }
    };

    print_metadata(value);

    let columns = if let Some(f) = fields {
        f.to_vec()
    } else {
        select_columns(&items)
    };
    if columns.is_empty() {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        );
        return;
    }

    let mut builder = Builder::default();
    builder.push_record(columns.iter().map(|c| c.bold().to_string()));

    for item in &items {
        let row: Vec<String> = columns
            .iter()
            .map(|col| {
                let raw = format_cell(item.get(col.as_str()));
                let truncated = truncate(&raw, MAX_CELL_WIDTH);
                colorize_value(col, &truncated)
            })
            .collect();
        builder.push_record(row);
    }

    let mut table = builder.build();
    table.with(Style::rounded());
    println!("{table}");
}

fn print_single_object(obj: &serde_json::Map<String, Value>) {
    let mut builder = Builder::default();
    builder.push_record(["Field".bold().to_string(), "Value".bold().to_string()]);

    for (key, val) in obj {
        let raw = format_cell(Some(val));
        let display = colorize_value(key, &truncate(&raw, 80));
        builder.push_record([key.dimmed().to_string(), display]);
    }

    let mut table = builder.build();
    table.with(Style::rounded());
    println!("{table}");
}

fn print_metadata(value: &Value) {
    if let Some(total) = value.get("totalCount").or(value.get("total"))
        && let Some(n) = total.as_u64()
    {
        eprintln!("{} {n} total", "::".dimmed());
    }
}

// ---------------------------------------------------------------------------
// Status / state / priority colorization
// ---------------------------------------------------------------------------

fn colorize_value(column: &str, value: &str) -> String {
    if value.is_empty() {
        return value.to_string();
    }

    let upper = value.to_uppercase();

    match column {
        "status" | "state" => colorize_status(&upper, value),
        "priority" | "severity" => colorize_priority(&upper, value),
        "type" => colorize_type(&upper, value),
        "id" => value.dimmed().to_string(),
        "createdAt" | "updatedAt" | "reportTime" | "resolvedOn" => format_timestamp(value),
        _ => value.to_string(),
    }
}

fn colorize_status(upper: &str, original: &str) -> String {
    match upper {
        // Alert states
        "PENDING" => original.yellow().bold().to_string(),
        "ACCEPTED" => original.cyan().bold().to_string(),
        "RESOLVED" => original.green().to_string(),

        // Incident states
        "INVESTIGATING" => original.yellow().bold().to_string(),
        "IDENTIFIED" => original.cyan().bold().to_string(),
        "MONITORING" => original.blue().to_string(),

        // Service states
        "OPERATIONAL" => original.green().to_string(),
        "DEGRADED" | "DEGRADED_PERFORMANCE" => original.yellow().to_string(),
        "PARTIAL_OUTAGE" | "PARTIAL" => original.red().to_string(),
        "MAJOR_OUTAGE" | "MAJOR" => original.red().bold().to_string(),
        "UNDER_MAINTENANCE" | "MAINTENANCE" => original.blue().to_string(),

        // Generic
        "ACTIVE" | "ENABLED" | "UP" | "OK" | "HEALTHY" => original.green().to_string(),
        "INACTIVE" | "DISABLED" | "DOWN" | "UNHEALTHY" => original.red().to_string(),
        "PAUSED" | "SUSPENDED" => original.yellow().to_string(),

        _ => original.to_string(),
    }
}

fn colorize_priority(upper: &str, original: &str) -> String {
    match upper {
        "HIGH" | "CRITICAL" | "P1" | "SEV1" => original.red().bold().to_string(),
        "MEDIUM" | "WARNING" | "P2" | "SEV2" => original.yellow().bold().to_string(),
        "LOW" | "INFO" | "P3" | "SEV3" => original.cyan().to_string(),
        _ => original.to_string(),
    }
}

fn colorize_type(upper: &str, original: &str) -> String {
    match upper {
        "ALERT" | "INCIDENT" => original.red().to_string(),
        "MAINTENANCE" | "SCHEDULED" => original.blue().to_string(),
        "INFORMATIONAL" | "INFO" => original.cyan().to_string(),
        _ => original.to_string(),
    }
}

fn format_timestamp(value: &str) -> String {
    // Try to make ISO timestamps more readable
    // "2024-03-22T14:30:00.000Z" -> "2024-03-22 14:30:00"
    let cleaned = value
        .replace('T', " ")
        .trim_end_matches('Z')
        .trim_end_matches(".000")
        .to_string();
    cleaned.dimmed().to_string()
}

// ---------------------------------------------------------------------------
// Item extraction & column selection
// ---------------------------------------------------------------------------

fn extract_items(value: &Value) -> Option<Vec<&Value>> {
    if let Some(arr) = value.as_array() {
        return Some(arr.iter().collect());
    }

    for key in &["items", "results", "data"] {
        if let Some(arr) = value.get(key).and_then(|v| v.as_array()) {
            return Some(arr.iter().collect());
        }
    }

    if let Some(obj) = value.as_object() {
        for (_, v) in obj {
            if let Some(arr) = v.as_array()
                && !arr.is_empty()
                && arr[0].is_object()
            {
                return Some(arr.iter().collect());
            }
        }
    }

    None
}

fn select_columns(items: &[&Value]) -> Vec<String> {
    let first = match items.first().and_then(|v| v.as_object()) {
        Some(obj) => obj,
        None => return Vec::new(),
    };

    let all_keys: Vec<String> = first.keys().cloned().collect();

    let mut selected: Vec<String> = PREFERRED_COLUMNS
        .iter()
        .filter(|&&col| first.contains_key(col))
        .map(|&s| s.to_string())
        .collect();

    for key in &all_keys {
        if selected.len() >= MAX_COLUMNS {
            break;
        }
        if selected.contains(key) {
            continue;
        }
        if let Some(val) = first.get(key.as_str())
            && !val.is_object()
            && !val.is_array()
        {
            selected.push(key.clone());
        }
    }

    selected
}

fn format_cell(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Array(arr)) => format!("[{} items]", arr.len()),
        Some(Value::Object(_)) => "{...}".to_string(),
    }
}

/// Truncate to `max` terminal columns, appending an ellipsis when it doesn't fit.
///
/// Measured in display width, not bytes: byte slicing panics mid-codepoint on
/// any multi-byte character (an umlaut in an alert summary was enough), and
/// counting `char`s still misaligns the table for CJK and emoji, which occupy
/// two columns each.
fn truncate(s: &str, max: usize) -> String {
    let width = s.width();
    if width <= max {
        return s.to_string();
    }
    // Reserve three columns for the ellipsis. `max` is a small constant here, but
    // guard anyway so a tight budget degrades instead of underflowing.
    let budget = max.saturating_sub(3);
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push_str("...");
    out
}

pub fn print_error(err: &anyhow::Error, format: OutputFormat) {
    // A refusal is structured data in every output mode: the caller that hit it
    // is the one least able to parse prose.
    if let Some(crate::errors::CliError::ConfirmationRequired { payload }) =
        err.downcast_ref::<crate::errors::CliError>()
    {
        eprintln!(
            "{}",
            serde_json::to_string_pretty(payload.as_ref()).unwrap_or_default()
        );
        return;
    }

    match format {
        OutputFormat::Table => {
            eprintln!("{} {err}", "Error:".red().bold());
            for cause in err.chain().skip(1) {
                eprintln!("  {} {cause}", "caused by:".dimmed());
            }
        }
        _ => {
            let error_json = serde_json::json!({
                "error": {
                    "message": format!("{err}"),
                }
            });
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&error_json).unwrap_or_default()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_leaves_short_values_untouched() {
        assert_eq!(truncate("short", 48), "short");
        // Exactly at the limit: still no ellipsis.
        let exact = "a".repeat(48);
        assert_eq!(truncate(&exact, 48), exact);
    }

    #[test]
    fn truncate_does_not_split_multibyte_chars() {
        // Regression: 25 umlauts are 50 *bytes* but only 25 columns. The old
        // byte-based check treated that as over-long and sliced at byte 45,
        // panicking mid-codepoint ("not a char boundary"). It fits — leave it be.
        let fits = "ä".repeat(25);
        assert_eq!(truncate(&fits, MAX_CELL_WIDTH), fits);

        // Genuinely too wide: truncate on a char boundary, never inside one.
        let long = "ä".repeat(60);
        let out = truncate(&long, MAX_CELL_WIDTH);
        assert!(out.ends_with("..."));
        assert_eq!(out.width(), MAX_CELL_WIDTH);
        assert!(out.chars().all(|c| c == 'ä' || c == '.'));
    }

    #[test]
    fn truncate_measures_display_width_not_bytes() {
        // Wide (2-column) characters must not overflow the cell.
        let wide = "世".repeat(40);
        let out = truncate(&wide, MAX_CELL_WIDTH);
        assert!(out.width() <= MAX_CELL_WIDTH);

        // An emoji straddling the budget boundary is dropped, never split.
        let emoji = format!("{}🚨", "a".repeat(44));
        let out = truncate(&emoji, MAX_CELL_WIDTH);
        assert!(out.width() <= MAX_CELL_WIDTH);
    }

    #[test]
    fn colored_cells_do_not_inflate_column_width() {
        // Guards the `tabled/ansi` feature: without it the escape sequences are
        // counted as visible width and every colored column misaligns.
        colored::control::set_override(true);
        let colored_row = colorize_value("status", "PENDING");
        colored::control::set_override(false);

        assert!(colored_row.contains('\u{1b}'), "expected ANSI escapes");
        let mut builder = Builder::default();
        builder.push_record(["status".to_string()]);
        builder.push_record([colored_row]);
        let mut table = builder.build();
        table.with(Style::rounded());
        let rendered = table.to_string();

        // Every border row must be the same display width as the header row.
        let widths: Vec<usize> = rendered.lines().map(console_width).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "misaligned table rows: {widths:?}\n{rendered}"
        );
    }

    /// Display width of a rendered line, ignoring ANSI escape sequences.
    fn console_width(line: &str) -> usize {
        let mut width = 0;
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                // Skip the CSI sequence up to and including its final byte.
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            width += c.width().unwrap_or(0);
        }
        width
    }
}
