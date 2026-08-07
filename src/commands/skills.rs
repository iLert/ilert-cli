//! `ilert skills list` / `ilert skills show <name>`
//!
//! Agent skills, retrievable without `npx`.
//!
//! **These commands write nothing to disk.** `show` prints the skill's markdown
//! to stdout and that is the entire mechanism: the caller is an agent already
//! executing our CLI, so the content lands in its context directly. There is no
//! reason to place a file in a discovery directory first — which is why there is
//! no platform-directory map, no agent sniffing, no `--force`, and no freshness
//! check here.
//!
//! Persistent project setup is a different job, and the ecosystem installers
//! already do it from the same `skills/` directory in this repo:
//!
//! ```text
//! npx skills add ilert/ilert-cli
//! ```
//!
//! Every skill is embedded at compile time. Retrieval is
//! therefore offline, release-pinned, and covered by the same review that gates
//! executable behaviour — no network, no cache, no repository-availability
//! failure mode.

use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde_json::Value;

use crate::errors::CliError;

/// The skills themselves — the single source of truth at runtime.
///
/// `skills/index.json` also describes this set, but it exists for the `npx
/// skills` ecosystem, which reads the repository rather than the binary. Nothing
/// here consults it: `list` reads the frontmatter of these documents, so the
/// catalog and the content cannot disagree. `manifest_agrees_with_the_skills`
/// keeps the committed manifest honest against the same frontmatter.
///
/// Adding a skill means adding its directory and a line here;
/// `scripts/generate-skills-index.sh` then regenerates the manifest.
const EMBEDDED: &[(&str, &str)] = &[
    (
        "migrate-from-opsgenie",
        include_str!("../../skills/migrate-from-opsgenie/SKILL.md"),
    ),
    (
        "migrate-from-pagerduty",
        include_str!("../../skills/migrate-from-pagerduty/SKILL.md"),
    ),
];

/// One `key: value` line from a skill's YAML frontmatter block.
///
/// Deliberately not a YAML parser: the frontmatter we accept is flat and
/// single-line, and `generate-skills-index.sh` reads it with the same rules.
fn frontmatter_field(body: &str, key: &str) -> Option<String> {
    let mut lines = body.lines();
    if lines.next()? != "---" {
        return None;
    }
    for line in lines.take_while(|l| l.trim_end() != "---") {
        if let Some((k, v)) = line.split_once(':')
            && k.trim() == key
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

fn body_for(name: &str) -> Option<&'static str> {
    EMBEDDED
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, body)| *body)
}

/// The catalog, derived from the embedded documents.
fn catalog() -> Result<Vec<Value>> {
    EMBEDDED
        .iter()
        .map(|(name, body)| {
            let description = frontmatter_field(body, "description").ok_or_else(|| {
                CliError::user(format!("Embedded skill '{name}' has no description."))
            })?;
            Ok(serde_json::json!({
                "name": name,
                "description": description,
                "path": format!("{name}/SKILL.md"),
            }))
        })
        .collect()
}

pub fn command() -> Command {
    Command::new("skills")
        .about("Agent skills bundled with this CLI")
        .arg_required_else_help(true)
        .after_help(
            "Examples:\n  \
             ilert skills list                       List available skills\n  \
             ilert skills show migrate-from-pagerduty  Print a skill's markdown\n\n\
             Skills print to stdout and are never written to disk. For a persistent\n\
             install into your agent's skills directory, use: npx skills add ilert/ilert-cli",
        )
        .subcommand(Command::new("list").about("List available skills"))
        .subcommand(
            Command::new("show")
                .about("Print a skill's markdown to stdout")
                .arg(Arg::new("name").required(true).help("Skill name")),
        )
}

/// `list` returns structured data for the shared output path; `show` returns raw
/// markdown, because the markdown is the payload rather than a rendering of it.
/// The caller decides how to emit each — including refusing `--jq` on the
/// markdown, which is not JSON and never will be.
pub enum SkillsOutput {
    Structured(Value),
    Markdown(&'static str),
}

pub fn handle(matches: &ArgMatches) -> Result<SkillsOutput> {
    match matches.subcommand() {
        Some(("list", _)) => Ok(SkillsOutput::Structured(Value::Array(catalog()?))),
        Some(("show", sub)) => {
            let name = sub.get_one::<String>("name").expect("required");
            let body = body_for(name).ok_or_else(|| {
                let available: Vec<&str> = EMBEDDED.iter().map(|(n, _)| *n).collect();
                CliError::user(format!(
                    "Unknown skill: '{name}'. Available: {}",
                    available.join(", ")
                ))
            })?;
            Ok(SkillsOutput::Markdown(body))
        }
        _ => Err(CliError::user("Usage: ilert skills <list|show>").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed manifest, read only by these tests. Runtime never sees it.
    const MANIFEST: &str = include_str!("../../skills/index.json");

    #[test]
    fn every_skill_carries_the_required_frontmatter() {
        for (name, body) in EMBEDDED {
            assert_eq!(
                frontmatter_field(body, "name").as_deref(),
                Some(*name),
                "{name}: frontmatter name must match the directory"
            );
            let description = frontmatter_field(body, "description")
                .unwrap_or_else(|| panic!("{name}: frontmatter needs a description"));
            assert!(
                !description.trim().is_empty(),
                "{name}: description is empty"
            );
        }
    }

    #[test]
    fn the_catalog_is_derived_from_the_documents() {
        let catalog = catalog().expect("catalog builds");
        assert_eq!(catalog.len(), EMBEDDED.len());
        for (entry, (name, body)) in catalog.iter().zip(EMBEDDED) {
            assert_eq!(entry["name"], serde_json::json!(name));
            assert_eq!(entry["path"], serde_json::json!(format!("{name}/SKILL.md")));
            assert_eq!(
                entry["description"],
                serde_json::json!(frontmatter_field(body, "description").unwrap())
            );
        }
    }

    /// `skills/index.json` is generated for the `npx skills` ecosystem and is
    /// not consulted at runtime — which is exactly why it can rot unnoticed.
    /// Every field it publishes is checked against the same frontmatter the CLI
    /// reads.
    #[test]
    fn manifest_agrees_with_the_skills() {
        let manifest: Value =
            serde_json::from_str(MANIFEST).expect("skills/index.json is valid JSON");
        let listed = manifest["skills"]
            .as_array()
            .expect("skills/index.json has a 'skills' array");

        assert_eq!(
            listed.len(),
            EMBEDDED.len(),
            "skills/index.json lists {} skills but {} are embedded — \
             run scripts/generate-skills-index.sh and add the include_str! line",
            listed.len(),
            EMBEDDED.len()
        );

        for (name, body) in EMBEDDED {
            let entry = listed
                .iter()
                .find(|e| e["name"] == serde_json::json!(name))
                .unwrap_or_else(|| {
                    panic!(
                        "skills/index.json does not list '{name}' — \
                         run scripts/generate-skills-index.sh"
                    )
                });
            assert_eq!(
                entry["path"],
                serde_json::json!(format!("{name}/SKILL.md")),
                "{name}: manifest path does not point at the embedded document"
            );
            assert_eq!(
                entry["description"],
                serde_json::json!(frontmatter_field(body, "description").unwrap()),
                "{name}: manifest description has drifted from the frontmatter"
            );
        }
    }

    #[test]
    fn frontmatter_stops_at_the_closing_marker() {
        let body = "---\nname: x\n---\n\ndescription: not frontmatter\n";
        assert_eq!(frontmatter_field(body, "name").as_deref(), Some("x"));
        assert_eq!(frontmatter_field(body, "description"), None);
        // A document without frontmatter yields nothing rather than guessing.
        assert_eq!(frontmatter_field("# Title\nname: x\n", "name"), None);
    }
}
