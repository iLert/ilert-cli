use std::io::{self, IsTerminal, Write};

use colored::Colorize;
use serde_json::Value;
use tabled::builder::Builder;
use tabled::settings::Style;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::sanitize::terminal_text;

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
                // Table mode fell through to raw JSON. serde escapes C0/C1
                // inside strings but emits bidi controls verbatim, so this is
                // not a no-op even though it usually looks like one.
                println!("{}", terminal_text(&value.to_string()));
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
            terminal_text(&serde_json::to_string_pretty(value).unwrap_or_default())
        );
        return;
    }

    let mut builder = Builder::default();
    // Headings are JSON keys off the wire, so they are no more trustworthy than
    // the values under them.
    builder.push_record(columns.iter().map(|c| terminal_text(c).bold().to_string()));

    for item in &items {
        let row: Vec<String> = columns
            .iter()
            .map(|col| {
                // Escape, then measure, then colour. Sanitizing after truncation
                // would let a cell be cut in the middle of an escape sequence
                // (and cut the part that made it look harmless), and sanitizing
                // after colouring would mangle our own ANSI codes instead of
                // the content's.
                let raw = terminal_text(&format_cell(item.get(col.as_str())));
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
        let raw = terminal_text(&format_cell(Some(val)));
        let display = colorize_value(key, &truncate(&raw, 80));
        builder.push_record([terminal_text(key).dimmed().to_string(), display]);
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
pub(crate) fn truncate(s: &str, max: usize) -> String {
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

    let http = match err.downcast_ref::<crate::errors::CliError>() {
        Some(crate::errors::CliError::Http {
            status, details, ..
        }) => Some((*status, details)),
        _ => None,
    };

    match format {
        OutputFormat::Table => {
            // The message is very often the server's, and a `\r` in it would
            // let a failure overwrite the line above with whatever it liked.
            eprintln!(
                "{} {}",
                "Error:".red().bold(),
                terminal_text(&err.to_string())
            );
            // The API's own error code says what kind of failure this is —
            // FEATURE_REQUIRED and QUOTA_EXCEEDED are plan problems, KEY_ERROR
            // is a credential problem — and the message alone often does not.
            if let Some((_, details)) = http {
                let details = details.as_ref().filter(|d| d.is_object());
                let code = details.and_then(|d| error_field(d, "code"));
                let detailed = details.and_then(|d| error_field(d, "detailedCode"));
                if let Some(code) = code {
                    let suffix = detailed.map(|d| format!(" / {d}")).unwrap_or_default();
                    eprintln!(
                        "  {} {}",
                        "code:".dimmed(),
                        terminal_text(&format!("{code}{suffix}"))
                    );
                }
            }
            for cause in err.chain().skip(1) {
                eprintln!(
                    "  {} {}",
                    "caused by:".dimmed(),
                    terminal_text(&cause.to_string())
                );
            }
        }
        _ => {
            let mut error = serde_json::Map::new();
            error.insert("message".into(), serde_json::json!(format!("{err}")));

            // A machine reading this needs to branch on the API's `code`, not
            // parse the prose. The spec documents no error shape at all, so the
            // body is the only place these live — pass it through rather than
            // reducing every failure to a sentence.
            if let Some((status, details)) = http {
                error.insert("status".into(), serde_json::json!(status));
                // Only a JSON *object* is an API error envelope worth passing
                // on. A gateway or a wrong path answers with an HTML page, and
                // relaying ten kilobytes of markup as "details" buries the one
                // line that says what went wrong.
                if let Some(details) = details.as_ref().filter(|d| d.is_object()) {
                    for field in ["code", "detailedCode"] {
                        if let Some(value) = error_field(details, field) {
                            error.insert(field.into(), serde_json::json!(value));
                        }
                    }
                    error.insert("details".into(), crate::preview::redact_body(details));
                }
            }

            let error_json = serde_json::json!({ "error": error });
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&error_json).unwrap_or_default()
            );
        }
    }
}

/// A string field of an API error body, if the body is an object that has it.
fn error_field(details: &serde_json::Value, name: &str) -> Option<String> {
    details.get(name)?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let _colors = crate::testutil::colors(true);
        let colored_row = colorize_value("status", "PENDING");

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

    // -----------------------------------------------------------------------
    // Terminal escapes in untrusted content
    // -----------------------------------------------------------------------

    /// Capture what `print_table` would build, without the stdout round-trip.
    /// Mirrors the cell pipeline exactly: escape, truncate, colorize.
    fn rendered_cell(column: &str, value: &Value) -> String {
        let raw = terminal_text(&format_cell(Some(value)));
        colorize_value(column, &truncate(&raw, MAX_CELL_WIDTH))
    }

    #[test]
    fn a_table_cell_cannot_carry_an_ansi_colour_sequence() {
        let cell = rendered_cell("summary", &json!("\u{1b}[31mDANGER\u{1b}[0m"));
        assert!(!cell.contains('\u{1b}'), "{cell:?}");
        assert!(cell.contains("\\u{001B}"));
    }

    /// The one that reaches past the terminal window.
    #[test]
    fn a_table_cell_cannot_write_the_clipboard_with_osc_52() {
        let cell = rendered_cell("summary", &json!("\u{1b}]52;c;cm0gLXJmIC8=\u{7}"));
        assert!(!cell.contains('\u{1b}'), "{cell:?}");
        assert!(!cell.contains('\u{7}'), "BEL terminator survived: {cell:?}");
    }

    #[test]
    fn a_table_cell_cannot_use_a_c1_introducer_instead_of_esc() {
        let cell = rendered_cell("summary", &json!("a\u{9b}2Jb"));
        assert!(!cell.contains('\u{9b}'), "{cell:?}");
        assert!(cell.contains("\\u{009B}"));
    }

    #[test]
    fn a_table_cell_cannot_forge_extra_rows_with_newlines() {
        let cell = rendered_cell("summary", &json!("real\n│ fake │\rgone"));
        assert!(!cell.contains('\n'), "{cell:?}");
        assert!(!cell.contains('\r'), "{cell:?}");
        assert_eq!(cell, "real\\n│ fake │\\rgone");
    }

    #[test]
    fn a_table_cell_cannot_reorder_itself_with_bidi_controls() {
        let cell = rendered_cell("summary", &json!("\u{202E}moc.elpmaxe.live\u{202C}"));
        assert!(!cell.contains('\u{202E}'), "{cell:?}");
        assert!(!cell.contains('\u{202C}'), "{cell:?}");
    }

    #[test]
    fn ordinary_unicode_in_a_cell_is_left_alone() {
        for text in [
            "Störung in München",
            "重大なアラート",
            "🚨 disk full",
            "a/b_c-d.e",
        ] {
            assert_eq!(rendered_cell("summary", &json!(text)), text);
        }
    }

    /// Escaping has to come first: truncating a cell that still contains an
    /// escape sequence can cut it in the middle, and the half that survives is
    /// the half the terminal acts on.
    #[test]
    fn escaping_happens_before_truncation() {
        let payload = format!("{}\u{1b}[31m", "a".repeat(MAX_CELL_WIDTH));
        let cell = rendered_cell("summary", &json!(payload));
        assert!(!cell.contains('\u{1b}'));
        assert_eq!(cell.width(), MAX_CELL_WIDTH);
    }

    /// The flip side: our own colours are added last and must come through.
    #[test]
    fn the_clis_own_colours_survive_sanitization() {
        let _colors = crate::testutil::colors(true);
        let cell = rendered_cell("status", &json!("PENDING"));

        assert!(
            cell.contains('\u{1b}'),
            "expected our own ANSI codes: {cell:?}"
        );
        assert!(cell.contains("PENDING"));
    }

    /// A column heading is a JSON key, which is no more trustworthy than the
    /// value beneath it.
    #[test]
    fn a_column_heading_is_escaped_too() {
        let heading = terminal_text("id\u{1b}[2J");
        assert!(!heading.contains('\u{1b}'), "{heading:?}");
    }

    #[test]
    fn machine_readable_output_is_left_byte_for_byte() {
        // JSON/NDJSON/raw are a contract with a program: serde's own escaping
        // is the right escaping there, and rewriting the payload would corrupt
        // data the caller asked for verbatim.
        let value = json!({"summary": "\u{1b}[31mred\u{1b}[0m\nline"});
        let rendered = serde_json::to_string(&value).unwrap();
        assert!(rendered.contains("\\u001b"), "{rendered}");
        assert!(rendered.contains("\\n"), "{rendered}");
        // Round-trips to exactly what the server sent.
        let back: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(back, value);
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
