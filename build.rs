//! Build-time preprocessing for the bundled agent skills.
//!
//! `skills/` is the single source of truth. Every build derives two things from
//! it, so neither can be forgotten when a skill is added, renamed or removed:
//!
//! * `$OUT_DIR/skills.rs` — the `EMBEDDED` table `src/commands/skills.rs`
//!   includes. `include_str!` puts each document in the binary, so retrieval
//!   stays offline and release-pinned.
//! * `skills/index.json` — the committed manifest the `npx skills` ecosystem
//!   reads from the repository. Nothing at runtime consults it; it is written
//!   here only so it cannot drift from the documents it claims to describe.
//!
//! The frontmatter rules are deliberately the same flat, single-line ones
//! `src/commands/skills.rs` applies at runtime.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets this"));
    let skills_dir = root.join("skills");

    // Watching the directory catches an added or removed skill; watching each
    // document catches an edit to one that already exists.
    println!("cargo:rerun-if-changed={}", skills_dir.display());
    println!("cargo:rerun-if-changed=build.rs");

    let skills = collect(&skills_dir);
    assert!(
        !skills.is_empty(),
        "no skills/*/SKILL.md found under {}",
        skills_dir.display()
    );
    for skill in &skills {
        println!("cargo:rerun-if-changed={}", skill.path.display());
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets this"));
    fs::write(out_dir.join("skills.rs"), embedded_table(&skills))
        .expect("write $OUT_DIR/skills.rs");

    write_if_changed(&skills_dir.join("index.json"), &manifest(&skills));
}

struct Skill {
    name: String,
    description: String,
    path: PathBuf,
}

/// Every `skills/<dir>/SKILL.md`, in a stable order.
fn collect(skills_dir: &Path) -> Vec<Skill> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(skills_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", skills_dir.display()))
        .map(|entry| entry.expect("read skills/ entry").path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();

    dirs.into_iter()
        .filter(|dir| dir.join("SKILL.md").is_file())
        .map(|dir| {
            let path = dir.join("SKILL.md");
            let body = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            let dir_name = dir
                .file_name()
                .expect("a directory has a name")
                .to_str()
                .unwrap_or_else(|| panic!("{dir:?}: directory name is not UTF-8"));

            let name = field(&body, "name")
                .unwrap_or_else(|| panic!("{path:?}: frontmatter is missing a 'name'"));
            let description = field(&body, "description")
                .unwrap_or_else(|| panic!("{path:?}: frontmatter is missing a 'description'"));

            assert!(
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    && !name.starts_with('-')
                    && !name.ends_with('-'),
                "{path:?}: name '{name}' must be lowercase letters, digits and hyphens"
            );
            assert!(
                name == dir_name,
                "{path:?}: name '{name}' does not match its directory '{dir_name}'"
            );
            assert!(!description.is_empty(), "{path:?}: description is empty");

            Skill {
                name,
                description,
                path,
            }
        })
        .collect()
}

/// One `key: value` line from the YAML frontmatter block at the top of a file.
fn field(body: &str, key: &str) -> Option<String> {
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

/// The `(name, body)` table, with the document contents pulled in by the
/// compiler rather than copied through this script.
fn embedded_table(skills: &[Skill]) -> String {
    let mut out = String::from("const EMBEDDED: &[(&str, &str)] = &[\n");
    for skill in skills {
        writeln!(
            out,
            "    ({}, include_str!({})),",
            quoted(&skill.name),
            quoted(skill.path.to_str().expect("skill path is UTF-8"))
        )
        .expect("writing to a String cannot fail");
    }
    out.push_str("];\n");
    out
}

fn manifest(skills: &[Skill]) -> String {
    let mut out = String::from("{\n  \"version\": 1,\n  \"skills\": [\n");
    for (i, skill) in skills.iter().enumerate() {
        let separator = if i + 1 == skills.len() { "\n" } else { ",\n" };
        write!(
            out,
            "    {{\n      \"name\": {},\n      \"description\": {},\n      \"path\": {}\n    }}{separator}",
            quoted(&skill.name),
            quoted(&skill.description),
            quoted(&format!("{}/SKILL.md", skill.name)),
        )
        .expect("writing to a String cannot fail");
    }
    out.push_str("  ]\n}\n");
    out
}

/// A quoted literal, valid in both JSON and Rust. Frontmatter is single-line,
/// so quote and backslash are the only characters either target needs escaped —
/// anything else is rejected loudly rather than encoded, since a control
/// character in a name or description is a mistake, not something to publish.
fn quoted(value: &str) -> String {
    assert!(
        !value.chars().any(char::is_control),
        "frontmatter value contains a control character: {value:?}"
    );
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Rewrite only on a real change: the manifest lives inside the directory this
/// script watches, so an unconditional write would ask cargo for another build.
fn write_if_changed(path: &Path, contents: &str) {
    if fs::read_to_string(path).is_ok_and(|current| current == contents) {
        return;
    }
    // The binary does not depend on this file, so a read-only checkout (a
    // vendored or packaged build) should warn rather than fail the build. The
    // `manifest_agrees_with_the_skills` test is what catches a stale commit.
    if let Err(e) = fs::write(path, contents) {
        println!("cargo:warning=could not update {}: {e}", path.display());
    }
}
