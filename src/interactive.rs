use std::io::IsTerminal;

use anyhow::Result;
use colored::Colorize;
use dialoguer::{Confirm, Input, Password, Select};
use serde_json::Value;

use crate::errors::CliError;

/// Attempt to build a request body interactively from an OpenAPI schema.
/// Returns None if prompting is not appropriate here, or if the user cancels.
///
/// A TTY alone is not enough: an agent can be attached to a pty and would hang
/// on a prompt it never renders. Both the detected mode and the terminal have to
/// agree before we ask a question.
pub fn prompt_for_body(schema: &Value, resource_name: &str, action: &str) -> Result<Option<Value>> {
    let mode_allows = crate::mode::detect()
        .map(|m| m.can_prompt())
        .unwrap_or(false);
    if !mode_allows || !std::io::stdin().is_terminal() {
        return Ok(None);
    }

    eprintln!(
        "\n{}",
        format!("  {} {} {}", "Creating".bold(), resource_name, action).cyan()
    );
    eprintln!(
        "{}",
        "  Fill in the fields below. Leave blank to skip optional fields.\n".dimmed()
    );

    let schema = resolve_schema(schema);

    let properties = match schema.get("properties").and_then(|v| v.as_object()) {
        Some(p) => p,
        None => return Ok(None),
    };

    let required: Vec<&str> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut body = serde_json::Map::new();

    for (field_name, field_schema) in properties {
        // Skip read-only fields
        if field_schema
            .get("readOnly")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }

        let is_required = required.contains(&field_name.as_str());
        let field_type = field_schema
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("string");

        // Skip complex nested objects/arrays for now (could recurse later)
        if field_type == "object" {
            continue;
        }

        let description = field_schema
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let enum_values = field_schema
            .get("enum")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            });

        let value = if is_secret_field(field_name, field_schema) {
            // Asked for with the echo off whatever the schema calls it: a
            // credential typed in the clear stays in the scrollback, and on a
            // shared screen it never needed to be there at all.
            prompt_secret(field_name, description, is_required)?
        } else if let Some(ref options) = enum_values {
            prompt_enum(field_name, description, options, is_required)?
        } else {
            match field_type {
                "boolean" => prompt_bool(field_name, description, is_required)?,
                "integer" | "number" => prompt_number(field_name, description, is_required)?,
                "array" => prompt_array(field_name, description, field_schema, is_required)?,
                _ => prompt_string(field_name, description, is_required)?,
            }
        };

        if let Some(v) = value {
            body.insert(field_name.clone(), v);
        }
    }

    if body.is_empty() {
        return Ok(None);
    }

    // Confirm before sending. The same redaction the `--dry-run` envelope uses:
    // there is no point taking the value in hidden if the confirmation step
    // prints it back out.
    eprintln!();
    eprintln!("{}", "  Request body:".bold());
    for line in preview_lines(&Value::Object(body.clone()))? {
        eprintln!("    {}", line.dimmed());
    }
    eprintln!();

    let confirmed = Confirm::new()
        .with_prompt("  Send this request?")
        .default(true)
        .interact()?;

    if !confirmed {
        return Err(CliError::user("Cancelled").into());
    }

    Ok(Some(Value::Object(body)))
}

/// The confirmation preview, one escaped line at a time.
///
/// Serializing is not enough on its own. `serde_json` escapes the C0 range
/// inside strings, so `ESC` and `BEL` arrive as `` and `` — but C1
/// (U+009B is CSI, U+009D is OSC) and the bidi controls are emitted verbatim as
/// UTF-8, and a terminal decoding UTF-8 dispatches those the same way it
/// dispatches `ESC [` and `ESC ]`.
///
/// Not everything in this body was typed by the person reading it: property
/// names are schema keys, and an enum field holds the exact value the spec
/// offered — [`prompt_enum`] deliberately sends the original rather than the
/// escaped form, so the spec's bytes reach here intact.
///
/// This is the one JSON rendering in the CLI that is prose rather than payload:
/// it is a question on stderr, and no program consumes it. `-o json` and the
/// `--dry-run` envelope stay byte-faithful.
fn preview_lines(body: &Value) -> Result<Vec<String>> {
    let rendered = serde_json::to_string_pretty(&crate::preview::redact_body(body))?;
    Ok(rendered
        .lines()
        .map(crate::sanitize::terminal_text)
        .collect())
}

/// Whether this field holds a credential, by the name the API gave it or by the
/// schema saying so outright.
///
/// The name test is [`crate::preview::is_sensitive_body_key`], so the prompt and
/// the preview cannot disagree about which fields are secret — a field hidden on
/// the way in but printed on the way out would be worse than either.
fn is_secret_field(name: &str, schema: &Value) -> bool {
    if crate::preview::is_sensitive_body_key(name) {
        return true;
    }
    schema.get("format").and_then(|f| f.as_str()) == Some("password")
}

fn prompt_secret(name: &str, description: &str, required: bool) -> Result<Option<Value>> {
    let label = field_label(name, description, required);

    let input = Password::new()
        .with_prompt(label)
        .allow_empty_password(!required)
        .interact()?;

    if input.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Value::String(input)))
    }
}

fn prompt_string(name: &str, description: &str, required: bool) -> Result<Option<Value>> {
    let label = field_label(name, description, required);

    let input: String = Input::new()
        .with_prompt(label)
        .allow_empty(!required)
        .interact_text()?;

    if input.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Value::String(input)))
    }
}

fn prompt_number(name: &str, description: &str, required: bool) -> Result<Option<Value>> {
    let label = field_label(name, description, required);

    let input: String = Input::new()
        .with_prompt(label)
        .allow_empty(!required)
        .interact_text()?;

    if input.is_empty() {
        return Ok(None);
    }

    if let Ok(n) = input.parse::<i64>() {
        Ok(Some(Value::Number(n.into())))
    } else if let Ok(n) = input.parse::<f64>() {
        Ok(serde_json::Number::from_f64(n).map(Value::Number))
    } else {
        Err(CliError::user(format!("Invalid number: {input}")).into())
    }
}

fn prompt_bool(name: &str, description: &str, _required: bool) -> Result<Option<Value>> {
    let label = field_label(name, description, false);

    let result = Confirm::new()
        .with_prompt(label)
        .default(false)
        .interact_opt()?;

    Ok(result.map(Value::Bool))
}

fn prompt_enum(
    name: &str,
    description: &str,
    options: &[String],
    required: bool,
) -> Result<Option<Value>> {
    if options.is_empty() {
        return prompt_string(name, description, required);
    }

    let label = field_label(name, description, required);

    // Escaped for the menu, but the *original* is what gets sent: the enum
    // member is a value the API defined and has to match on the way back, so
    // rewriting it to make it printable would post something the server never
    // offered. Display and payload are kept as two lists over one index.
    let mut items: Vec<String> = options
        .iter()
        .map(|o| crate::sanitize::terminal_text(o))
        .collect();
    if !required {
        items.insert(0, "(skip)".to_string());
    }

    let selection = Select::new()
        .with_prompt(label)
        .items(&items)
        .default(0)
        .interact()?;

    if !required && selection == 0 {
        Ok(None)
    } else {
        // "(skip)" occupies index 0 when it is present and was handled above, so
        // this shift lands on a real option in both cases.
        let index = if required { selection } else { selection - 1 };
        Ok(Some(Value::String(options[index].clone())))
    }
}

fn prompt_array(
    name: &str,
    description: &str,
    schema: &Value,
    required: bool,
) -> Result<Option<Value>> {
    let label = field_label(name, description, required);
    eprintln!("  {}", format!("{label} (comma-separated)").dimmed());

    let input: String = Input::new()
        .with_prompt(format!("  {}", crate::sanitize::terminal_text(name)))
        .allow_empty(!required)
        .interact_text()?;

    if input.is_empty() {
        return Ok(None);
    }

    let item_type = schema
        .get("items")
        .and_then(|i| i.get("type"))
        .and_then(|t| t.as_str())
        .unwrap_or("string");

    let values: Vec<Value> = input
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| match item_type {
            "integer" => s
                .parse::<i64>()
                .map(|n| Value::Number(n.into()))
                .unwrap_or(Value::String(s.to_string())),
            "number" => s
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .unwrap_or(Value::String(s.to_string())),
            _ => Value::String(s.to_string()),
        })
        .collect();

    Ok(Some(Value::Array(values)))
}

/// The prompt line for one field.
///
/// Both halves come out of the OpenAPI document, so both are escaped before
/// anything else happens to them — a property name or description carrying a
/// CSI sequence would otherwise redraw the question the user is answering.
/// Truncation runs on the escaped text, and by display width rather than by
/// byte index: `&description[..57]` panics outright the moment a description
/// contains a character that is not ASCII.
fn field_label(name: &str, description: &str, required: bool) -> String {
    let name = crate::sanitize::terminal_text(name);
    let description = crate::sanitize::terminal_text(description);

    let req_marker = if required {
        " *".red().bold().to_string()
    } else {
        String::new()
    };

    if description.is_empty() || description == name {
        format!("  {name}{req_marker}")
    } else {
        let short_desc = crate::output::truncate(&description, 60);
        format!("  {name}{req_marker} {}", short_desc.dimmed())
    }
}

/// Schema refs are already resolved at index build time.
/// This is a passthrough kept for safety — if somehow an unresolved allOf slips through.
fn resolve_schema(schema: &Value) -> &Value {
    if let Some(all_of) = schema.get("allOf").and_then(|v| v.as_array()) {
        for item in all_of {
            if item.get("properties").is_some() {
                return item;
            }
        }
    }
    schema
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({ "type": "string" })
    }

    #[test]
    fn credential_fields_are_taken_with_the_echo_off() {
        for name in [
            "password",
            "apiKey",
            "api_key",
            "integrationKey",
            "routingKey",
            "webhookSecret",
            "refreshToken",
        ] {
            assert!(is_secret_field(name, &schema()), "{name} was echoed");
        }
    }

    #[test]
    fn a_schema_may_say_so_itself() {
        // Nothing in the name suggests it; the schema does.
        assert!(is_secret_field(
            "value",
            &json!({ "type": "string", "format": "password" })
        ));
    }

    #[test]
    fn ordinary_fields_stay_visible() {
        for name in ["summary", "name", "email", "keyRotationDays"] {
            assert!(!is_secret_field(name, &schema()), "{name} was hidden");
        }
    }

    /// The label is built from spec text, so it is the schema's chance to write
    /// to the terminal directly.
    #[test]
    fn a_prompt_label_cannot_carry_an_escape_sequence() {
        // Colour off, so the only way an escape reaches the label is from the
        // spec text — `.dimmed()` and `.red()` would otherwise supply their own.
        let _colors = crate::testutil::colors(false);

        let label = field_label("name\u{1b}[2J", "\u{1b}]52;c;cm0gLXJmIC8=\u{7}", true);
        assert!(!label.contains('\u{1b}'), "{label:?}");
        assert!(!label.contains('\u{7}'), "{label:?}");

        let bidi = field_label("host", "\u{202E}moc.elpmaxe.live\u{202C}", false);
        assert!(!bidi.contains('\u{202E}'), "{bidi:?}");
    }

    /// Regression: the old label truncated by byte index, so a description over
    /// 60 *bytes* whose 57th byte fell inside a character panicked — while the
    /// prompt was up and the terminal was already in raw mode. Each of these is
    /// over 60 bytes; only some are over 60 columns.
    #[test]
    fn a_long_non_ascii_description_does_not_panic() {
        for desc in [
            "ü".repeat(40),                                   // 80 bytes, 40 columns
            "重".repeat(40),                                  // 120 bytes, 80 columns
            "🚨".repeat(40),                                  // 160 bytes, 80 columns
            format!("{}ü{}", "a".repeat(56), "b".repeat(20)), // splits at byte 57
        ] {
            let label = field_label("field", &desc, false);
            assert!(label.starts_with("  field "), "{label:?}");
        }
    }

    /// Shortening happens by display width and lands on a character boundary,
    /// so the result is still valid text.
    #[test]
    fn an_over_wide_description_is_shortened_cleanly() {
        let label = field_label("field", &"重".repeat(40), false);
        assert!(label.contains("..."), "{label:?}");
        assert!(label.contains('重'), "{label:?}");
        assert!(!label.contains('\u{fffd}'), "{label:?}");
    }

    #[test]
    fn a_short_description_is_left_whole() {
        let label = field_label("field", "Störung in München", false);
        assert!(label.contains("Störung in München"), "{label:?}");
        assert!(!label.contains("..."), "{label:?}");
    }

    /// `serde_json` escapes C0 but passes C1 and bidi through as UTF-8, so
    /// serializing is not by itself a defence. U+009B is CSI, U+009D opens an
    /// OSC and U+009C is the string terminator that closes it — together they
    /// are a complete clipboard write with no `ESC` anywhere in the bytes.
    #[test]
    fn the_confirmation_preview_escapes_what_serde_lets_through() {
        let _colors = crate::testutil::colors(false);

        let body = json!({
            "name\u{9b}2J": "a",
            "mode": "\u{9d}52;c;cm0gLXJmIC8=\u{9c}",
            "host": "\u{202E}moc.elpmaxe.live\u{202C}",
            "note": "\u{1b}[31mred",
        });

        let joined = preview_lines(&body).unwrap().join("\n");
        for c in [
            '\u{9b}', '\u{9d}', '\u{9c}', '\u{202E}', '\u{202C}', '\u{1b}',
        ] {
            assert!(!joined.contains(c), "{c:?} survived: {joined:?}");
        }
        assert!(joined.contains("\\u{009B}"), "{joined:?}");
        assert!(joined.contains("\\u{009D}"), "{joined:?}");
        assert!(joined.contains("\\u{009C}"), "{joined:?}");
        assert!(joined.contains("\\u{202E}"), "{joined:?}");
    }

    /// Proves the premise: serde alone leaves all three classes intact.
    #[test]
    fn serde_alone_would_not_have_caught_these() {
        let raw = serde_json::to_string(&json!({ "a": "\u{9b}\u{9d}\u{9c}\u{202E}" })).unwrap();
        for c in ['\u{9b}', '\u{9d}', '\u{9c}', '\u{202E}'] {
            assert!(raw.contains(c), "{c:?} was escaped after all: {raw:?}");
        }
    }

    #[test]
    fn an_ordinary_preview_is_still_readable_json() {
        let _colors = crate::testutil::colors(false);
        let lines = preview_lines(&json!({ "summary": "Störung in München" })).unwrap();
        assert_eq!(lines[0], "{");
        assert!(lines[1].contains("Störung in München"), "{lines:?}");
    }

    #[test]
    fn the_confirmation_preview_redacts_what_the_prompt_hid() {
        let body = json!({ "name": "webhook", "apiKey": "il1api-secret" });
        let rendered = serde_json::to_string(&crate::preview::redact_body(&body)).unwrap();
        assert!(!rendered.contains("il1api-secret"), "{rendered}");
        assert!(rendered.contains("webhook"), "{rendered}");
    }
}
