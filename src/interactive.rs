use std::io::IsTerminal;

use anyhow::Result;
use colored::Colorize;
use dialoguer::{Confirm, Input, Select};
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

        let value = if let Some(ref options) = enum_values {
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

    // Confirm before sending
    eprintln!();
    let preview = serde_json::to_string_pretty(&Value::Object(body.clone()))?;
    eprintln!("{}", "  Request body:".bold());
    for line in preview.lines() {
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
    let mut items = options.to_vec();
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
        // The "(skip)" entry is index 0 and was handled above, so whatever is
        // selected here is a real option in both cases.
        Ok(Some(Value::String(items[selection].clone())))
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
        .with_prompt(format!("  {name}"))
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

fn field_label(name: &str, description: &str, required: bool) -> String {
    let req_marker = if required {
        " *".red().bold().to_string()
    } else {
        String::new()
    };

    if description.is_empty() || description == name {
        format!("  {name}{req_marker}")
    } else {
        let short_desc = if description.len() > 60 {
            format!("{}...", &description[..57])
        } else {
            description.to_string()
        };
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
