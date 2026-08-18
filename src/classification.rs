//! How dangerous is a command?
//!
//! `read_only`, `destructive` and `idempotent` are independent semantic
//! properties. HTTP methods give us defaults, not proof: a `POST` may be
//! idempotent, and a non-`DELETE` action may still destroy data. So the method
//! table below is only a starting point — a spec can carry an
//! `x-ilert-cli-classification` extension per operation, and
//! [`OPERATION_OVERRIDES`] is the fallback for specs that cannot yet carry it.
//!
//! The confirmation flow in `cli.rs` gates on `destructive`, so this is the one
//! place that decides what needs a `--yes`.
//!
//! # Confirmation policy
//!
//! **A write is not destructive by default. `DELETE` is, and so is anything
//! named in [`OPERATION_OVERRIDES`].**
//!
//! `POST` and `PUT` therefore go through without confirmation unless listed.
//! This is a deliberate choice, not an oversight of the spec: creating an alert
//! source, updating a schedule or acknowledging an alert is the ordinary work
//! the CLI exists to do, and a `--yes` on every write trains people to pass it
//! unconditionally — at which point the flag protects nothing, including the
//! deletes it was meant for. Confirmation is worth something only while it stays
//! rare enough to read.
//!
//! An operation earns an override when the request destroys or overwrites state
//! that the caller did not name. The three shapes seen so far:
//!
//! - **Replace-semantics writes.** A `PUT` whose summary says *set* or *replace*
//!   discards whatever was there. The caller lists five subscribers; the four
//!   they omitted are removed, and nothing in the request said so.
//! - **Irreversible outward effects.** Invoking an alert action fires a webhook,
//!   opens a ticket, posts to chat. There is no second call that unsends it.
//! - **History rewrites.** Overriding outage history restates a public uptime
//!   record after the fact.
//!
//! Writes that publish (creating an incident, sending an event) are *not*
//! overridden. They are outward-facing and irreversible too, but they are also
//! the documented purpose of those commands, invoked deliberately, and gating
//! them would put a prompt in the middle of the primary workflow.
//!
//! The reverse direction matters as much: a `POST` that only queries — the
//! subscriber forecast, the user-by-email lookup — is marked `read_only` so the
//! preview does not describe a search as a write.

use anyhow::Result;
use serde_json::Value;

use crate::errors::CliError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classification {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
}

impl Classification {
    pub const fn new(read_only: bool, destructive: bool, idempotent: bool) -> Self {
        Self {
            read_only,
            destructive,
            idempotent,
        }
    }

    /// Default classification for an HTTP method (case-insensitive).
    ///
    /// | Method | read_only | destructive | idempotent |
    /// | --- | --- | --- | --- |
    /// | GET, HEAD | true | false | true |
    /// | POST | false | false | false |
    /// | PUT | false | false | true |
    /// | PATCH | false | false | false |
    /// | DELETE | false | true | true |
    /// | anything else | false | true | false |
    pub fn from_method(method: &str) -> Self {
        match method.to_ascii_uppercase().as_str() {
            "GET" | "HEAD" => Self::new(true, false, true),
            "POST" => Self::new(false, false, false),
            "PUT" => Self::new(false, false, true),
            "PATCH" => Self::new(false, false, false),
            "DELETE" => Self::new(false, true, true),
            // An unknown method is the one case where we know nothing at all:
            // it is not read-only, we cannot promise a retry is safe, and it
            // may well destroy data. `ilert api -X PURGE /x` therefore goes
            // through the confirmation gate rather than around it.
            _ => Self::new(false, true, false),
        }
    }

    /// Read-only and destructive are mutually exclusive by definition.
    pub fn is_consistent(&self) -> bool {
        !(self.read_only && self.destructive)
    }

    pub fn to_json(self) -> Value {
        serde_json::json!({
            "read_only": self.read_only,
            "destructive": self.destructive,
            "idempotent": self.idempotent,
        })
    }
}

/// Methods whose destructiveness the spec does not get a vote on.
///
/// `DELETE` says what it does, and a method we have never heard of could do
/// anything. Both are [`Classification::from_method`] destructive, and
/// [`clamp_to_method_floor`] will not let a document argue otherwise — see the
/// note there for why.
fn destructive_regardless_of_the_spec(method: &str) -> bool {
    !matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "POST" | "PUT" | "PATCH"
    )
}

/// Raise a classification back to what the HTTP method itself guarantees.
///
/// This has to run **last**, after every input that can lower the guard. Both
/// of those inputs are addressed by something the spec chooses: the extension
/// is read out of the operation object, and [`OPERATION_OVERRIDES`] is matched
/// on `operationId` alone. The ilert spec declares no `operationId`, so ours
/// are synthesized from method and path — but nothing stops a document from
/// declaring one, and a `DELETE` that names itself `post-incidents-publish-info`
/// would otherwise inherit that entry's `read_only` and skip confirmation
/// entirely. The method is the one part of an operation we do not take on trust,
/// so it gets the last word.
///
/// Idempotence is left alone: it says whether a retry is safe, not whether to
/// ask, and both answers are defensible for a `DELETE`.
fn clamp_to_method_floor(method: &str, requested: Classification) -> Classification {
    if !destructive_regardless_of_the_spec(method) {
        return requested;
    }

    Classification {
        // `read_only` is clamped alongside `destructive` rather than left to
        // fail the consistency check: a clamp that turns into a hard error is
        // just a slower way for a spec to break the CLI, and "this DELETE is a
        // read" is not a claim worth preserving in order to reject it.
        read_only: false,
        destructive: true,
        idempotent: requested.idempotent,
    }
}

/// Apply an `x-ilert-cli-classification` extension on top of a base
/// classification. Every field is optional, so a spec can flip a single
/// property without restating the rest.
///
/// ```json
/// "x-ilert-cli-classification": { "destructive": true }
/// ```
///
/// # What a spec may not do
///
/// The extension may raise the guard on an operation and it may lower it on a
/// `POST`/`PUT`/`PATCH` — that is the whole point of it, and those already go
/// through without confirmation by default, so nothing is given away.
///
/// It may **not** clear `destructive` on a `DELETE` or on a method we do not
/// recognise. The spec arrives over the network from the same host the request
/// would go to, so treating it as authority over the confirmation gate means
/// `"x-ilert-cli-classification": {"destructive": false, "readOnly": true}` on
/// `DELETE /alerts/{id}` turns `--yes` off for deletions and makes the preview
/// describe them as reads. A document cannot be allowed to disarm the check
/// that exists to protect the caller from it. Downgrades of that kind are
/// clamped silently rather than raised as an error: this runs while the command
/// tree is being built, and a spec bug should not stop the CLI from starting.
pub fn apply_extension(method: &str, base: Classification, op_value: &Value) -> Classification {
    let Some(ext) = op_value.get(EXTENSION_KEY).and_then(|v| v.as_object()) else {
        return base;
    };

    let field = |camel: &str, snake: &str, current: bool| -> bool {
        ext.get(camel)
            .or_else(|| ext.get(snake))
            .and_then(|v| v.as_bool())
            .unwrap_or(current)
    };

    clamp_to_method_floor(
        method,
        Classification {
            read_only: field("readOnly", "read_only", base.read_only),
            destructive: field("destructive", "destructive", base.destructive),
            idempotent: field("idempotent", "idempotent", base.idempotent),
        },
    )
}

pub const EXTENSION_KEY: &str = "x-ilert-cli-classification";

/// Checked-in overrides keyed by `operationId`, applied after the method
/// default and after any `x-ilert-cli-classification` extension.
///
/// This exists for operations the spec does not yet classify. Prefer adding
/// the extension to the spec; an entry here is a temporary bridge. Entries that
/// no longer match an operation in the spec fail `stale_overrides_are_rejected`.
/// Every entry is one operation reviewed by hand against the policy in the
/// module docs; `every_override_is_justified` keeps the reasoning attached.
///
/// The ilert spec declares no `operationId`, so these keys are the synthesized
/// `{method}-{slugified-path}` form — see [`crate::openapi::operation_id`].
pub const OPERATION_OVERRIDES: &[(&str, Classification)] = &[
    // -- Irreversible outward effects ---------------------------------------
    // POST /alerts/{id}/actions — invokes an alert action: fires the webhook,
    // opens the Jira issue, posts to the channel. Nothing undoes it.
    (
        "post-alerts-id-actions",
        Classification::new(false, true, false),
    ),
    // -- History rewrites ---------------------------------------------------
    // POST /service-outages/overrides — restates a service's outage history,
    // which is the uptime figure a status page shows the public.
    (
        "post-service-outages-overrides",
        Classification::new(false, true, false),
    ),
    // PUT /service-outages/overrides/{id} — same record, edited in place.
    (
        "put-service-outages-overrides-id",
        Classification::new(false, true, true),
    ),
    // -- Replace-semantics writes -------------------------------------------
    // PUT /services/{id}/private-subscribers — "Set subscribers": everyone
    // absent from the request is removed from the service.
    (
        "put-services-id-private-subscribers",
        Classification::new(false, true, true),
    ),
    // PUT /status-pages/{id}/private-subscribers — "Set subscribers", same
    // shape. The sibling POST *adds* and stays non-destructive.
    (
        "put-status-pages-id-private-subscribers",
        Classification::new(false, true, true),
    ),
    // PUT /escalation-policies/{id}/levels/{level} — "Replace an escalation
    // rule": drops whoever held that level. The failure is silent and it is a
    // paging path — you find out when nobody is woken up.
    (
        "put-escalation-policies-id-levels-level",
        Classification::new(false, true, true),
    ),
    // -- Reads behind a POST ------------------------------------------------
    // POST /incidents/publish-info — forecasts which subscribers and status
    // pages an incident would reach. Computes and returns; changes nothing.
    (
        "post-incidents-publish-info",
        Classification::new(true, false, true),
    ),
    // POST /users/search-email — a lookup by email, POSTed so the address stays
    // out of the query string.
    (
        "post-users-search-email",
        Classification::new(true, false, true),
    ),
];

/// Resolve the final classification for a spec operation.
///
/// Precedence: method default → `x-ilert-cli-classification` extension →
/// `overrides` entry for this `operationId`.
///
/// An inconsistent result is an error, not an assertion. A spec that claims an
/// operation is both read-only and destructive would otherwise ship a preview
/// that tells the caller a delete is safe, and `debug_assert!` says nothing at
/// all in the release binary people actually run.
pub fn resolve_with(
    method: &str,
    operation_id: &str,
    op_value: &Value,
    overrides: &[(&str, Classification)],
) -> Result<Classification> {
    let base = apply_extension(method, Classification::from_method(method), op_value);
    let resolved = overrides
        .iter()
        .find(|(id, _)| *id == operation_id)
        .map(|(_, c)| *c)
        .unwrap_or(base);
    // Reapplied after the override lookup, not just inside `apply_extension`:
    // the table is keyed on `operationId` with no regard for the method, so a
    // spec that picks the right id can borrow a read-only entry for a DELETE.
    let resolved = clamp_to_method_floor(method, resolved);

    if !resolved.is_consistent() {
        return Err(CliError::user(format!(
            "Operation '{operation_id}' ({method}) is classified as both read-only and \
             destructive, which cannot be true. Fix its '{EXTENSION_KEY}' extension in the \
             API spec, or its entry in OPERATION_OVERRIDES."
        ))
        .into());
    }

    Ok(resolved)
}

/// [`resolve_with`] against the shipped override table.
pub fn for_operation(method: &str, operation_id: &str, op_value: &Value) -> Result<Classification> {
    resolve_with(method, operation_id, op_value, OPERATION_OVERRIDES)
}

// ---------------------------------------------------------------------------
// Static commands
// ---------------------------------------------------------------------------

/// Static commands never pass through the OpenAPI index, so they carry explicit
/// metadata using the same type. Keys are the full command path as dispatched,
/// e.g. `"alerts ack"`.
///
/// Local-only mutations (writing a profile, storing a credential) are not
/// read-only, but they are not destructive either — nothing on the server
/// changes and the action is repeatable.
///
/// `cli::static_command_paths` enumerates the command tree and
/// `every_static_command_is_classified` fails if a command lands here without
/// an entry, so adding a static mutating command cannot silently skip the gate.
pub const STATIC_COMMANDS: &[(&str, Classification)] = &[
    // Read-only.
    ("auth whoami", Classification::new(true, false, true)),
    ("auth show", Classification::new(true, false, true)),
    ("config list", Classification::new(true, false, true)),
    ("config show", Classification::new(true, false, true)),
    ("completions", Classification::new(true, false, true)),
    ("ops list", Classification::new(true, false, true)),
    ("ops show", Classification::new(true, false, true)),
    ("on-call", Classification::new(true, false, true)),
    ("on-call now", Classification::new(true, false, true)),
    ("status", Classification::new(true, false, true)),
    ("version", Classification::new(true, false, true)),
    ("dashboard", Classification::new(true, false, true)),
    ("skills list", Classification::new(true, false, true)),
    ("skills show", Classification::new(true, false, true)),
    // Local mutations.
    ("auth login", Classification::new(false, false, true)),
    ("auth logout", Classification::new(false, false, true)),
    // Replaces this binary with the latest release. Not destructive in the
    // sense the confirmation gate means — no server state changes and any
    // version can be installed again — and repeating it lands on the same
    // release. It has its own consent gate in `commands::update`, because what
    // it needs to ask about is running the installer, not an API call.
    ("update", Classification::new(false, false, true)),
    ("config import", Classification::new(false, false, true)),
    // Remote mutations.
    ("event send", Classification::new(false, false, false)),
    ("heartbeat ping", Classification::new(false, false, true)),
    ("alerts ack", Classification::new(false, false, true)),
    ("alerts resolve", Classification::new(false, false, true)),
    ("alerts assign", Classification::new(false, false, true)),
];

/// Commands whose classification cannot be a constant because it depends on the
/// request the caller builds: `ilert api` takes the method from `-X`, and
/// `ilert ops run` takes it from the operation named on the command line. Both
/// resolve a real [`Classification`] per invocation before the gate runs.
#[cfg(test)]
pub const PER_INVOCATION_COMMANDS: &[&str] = &["api", "ops run"];

pub fn for_static_command(command: &str) -> Option<Classification> {
    STATIC_COMMANDS
        .iter()
        .find(|(name, _)| *name == command)
        .map(|(_, c)| *c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The real spec, also used by the e2e suite.
    const SPEC: &str = include_str!("../tests/fixtures/openapi.json");

    #[test]
    fn method_defaults_match_the_documented_table() {
        for m in ["GET", "HEAD", "get"] {
            assert_eq!(
                Classification::from_method(m),
                Classification::new(true, false, true)
            );
        }
        assert_eq!(
            Classification::from_method("POST"),
            Classification::new(false, false, false)
        );
        assert_eq!(
            Classification::from_method("PUT"),
            Classification::new(false, false, true)
        );
        assert_eq!(
            Classification::from_method("PATCH"),
            Classification::new(false, false, false)
        );
        assert_eq!(
            Classification::from_method("DELETE"),
            Classification::new(false, true, true)
        );
    }

    #[test]
    fn an_unknown_method_is_treated_as_destructive() {
        for m in ["PURGE", "TRUNCATE", "wipe"] {
            let c = Classification::from_method(m);
            assert!(c.destructive, "{m} should require confirmation");
            assert!(!c.read_only);
            assert!(!c.idempotent);
        }
    }

    #[test]
    fn extension_overrides_single_fields() {
        let op = json!({ "x-ilert-cli-classification": { "destructive": true } });
        let resolved = for_operation("POST", "purgeEverything", &op).unwrap();
        assert!(resolved.destructive);
        assert!(!resolved.read_only);
        // Untouched fields keep the method default.
        assert!(!resolved.idempotent);
    }

    /// The spec comes from the network. It may tighten the gate, never open it
    /// on the two methods whose danger is not the spec's to reinterpret.
    #[test]
    fn a_spec_cannot_talk_us_out_of_confirming_a_delete() {
        let disarm = json!({
            "x-ilert-cli-classification": { "destructive": false, "readOnly": true },
        });

        for method in ["DELETE", "delete", "PURGE", "TRUNCATE", "wipe"] {
            let c = for_operation(method, "someOp", &disarm).unwrap();
            assert!(c.destructive, "{method} lost its confirmation gate");
            assert!(!c.read_only, "{method} was relabelled as a read");
        }
    }

    /// The override table is looked up by `operationId` alone, so a document
    /// that names a `DELETE` after a known read-only operation would otherwise
    /// inherit that entry and skip the prompt. The floor is reapplied after the
    /// lookup, not only inside `apply_extension`.
    #[test]
    fn a_delete_cannot_borrow_a_read_only_operation_id() {
        // Every read-only entry we ship is a live key for this attack.
        let read_only_ids: Vec<&str> = OPERATION_OVERRIDES
            .iter()
            .filter(|(_, c)| c.read_only)
            .map(|(id, _)| *id)
            .collect();
        assert!(!read_only_ids.is_empty(), "test needs a read-only entry");

        for id in read_only_ids {
            for method in ["DELETE", "delete", "PURGE"] {
                let c = for_operation(method, id, &json!({})).unwrap();
                assert!(c.destructive, "{method} {id} lost its confirmation gate");
                assert!(!c.read_only, "{method} {id} was relabelled as a read");
            }
            // The entry still applies to the method it was written for.
            let (write_method, _) = id.split_once('-').unwrap();
            let c = for_operation(write_method, id, &json!({})).unwrap();
            assert!(
                c.read_only,
                "{id} lost its own override under {write_method}"
            );
        }
    }

    /// The other direction still works: a write the spec knows to be harmless
    /// stays unconfirmed, and one it knows to be dangerous gains the gate.
    #[test]
    fn a_spec_may_still_classify_the_methods_it_owns() {
        let relax = json!({ "x-ilert-cli-classification": { "readOnly": true } });
        for method in ["POST", "PUT", "PATCH"] {
            let c = for_operation(method, "searchOp", &relax).unwrap();
            assert!(c.read_only, "{method} should be allowed to declare a read");
            assert!(!c.destructive);
        }

        let tighten = json!({ "x-ilert-cli-classification": { "destructive": true } });
        assert!(
            for_operation("POST", "purgeOp", &tighten)
                .unwrap()
                .destructive
        );
    }

    /// A clamped `DELETE` keeps whatever the spec said about retry safety —
    /// only the two fields the gate reads are pinned.
    #[test]
    fn clamping_a_delete_leaves_idempotence_to_the_spec() {
        let op = json!({
            "x-ilert-cli-classification": { "destructive": false, "idempotent": false },
        });
        let c = for_operation("DELETE", "someOp", &op).unwrap();
        assert!(c.destructive);
        assert!(!c.idempotent);
    }

    #[test]
    fn extension_accepts_snake_case_too() {
        let op = json!({ "x-ilert-cli-classification": { "read_only": false } });
        assert!(!for_operation("GET", "weirdGet", &op).unwrap().read_only);
    }

    #[test]
    fn extension_is_ignored_when_absent_or_malformed() {
        assert_eq!(
            for_operation("PUT", "x", &json!({})).unwrap(),
            Classification::new(false, false, true)
        );
        assert_eq!(
            for_operation("PUT", "x", &json!({ "x-ilert-cli-classification": "yes" })).unwrap(),
            Classification::new(false, false, true)
        );
    }

    #[test]
    fn an_override_beats_the_method_default_and_the_extension() {
        // The shipped table is empty, so precedence is exercised by handing the
        // real resolver a table of its own — the same code path `for_operation`
        // takes, not a reimplementation of it.
        let table: &[(&str, Classification)] =
            &[("someOp", Classification::new(false, true, true))];

        // Method default alone would be POST => (false, false, false); the
        // extension would make it idempotent; the override wins over both.
        let op = json!({ "x-ilert-cli-classification": { "idempotent": true } });
        assert_eq!(
            resolve_with("POST", "someOp", &op, table).unwrap(),
            Classification::new(false, true, true)
        );

        // An operation the table does not name still resolves normally.
        assert_eq!(
            resolve_with("POST", "otherOp", &op, table).unwrap(),
            Classification::new(false, false, true)
        );
    }

    #[test]
    fn an_inconsistent_classification_is_rejected_in_every_build() {
        // Via the extension...
        let op = json!({ "x-ilert-cli-classification": { "destructive": true } });
        let err = for_operation("GET", "impossibleGet", &op).unwrap_err();
        assert!(err.to_string().contains("impossibleGet"));
        assert!(err.to_string().contains("read-only and destructive"));

        // ...and via an override.
        let table: &[(&str, Classification)] = &[("bad", Classification::new(true, true, true))];
        assert!(resolve_with("POST", "bad", &json!({}), table).is_err());

        // For a DELETE the same entry is clamped instead of rejected — the
        // method floor runs first and settles the contradiction on the safe
        // side, which beats refusing to build the command tree at all.
        let clamped = resolve_with("DELETE", "bad", &json!({}), table).unwrap();
        assert_eq!(clamped, Classification::new(false, true, true));
    }

    #[test]
    fn stale_overrides_are_rejected() {
        let spec: Value = serde_json::from_str(SPEC).expect("fixture spec is valid JSON");
        let known = operation_ids(&spec);
        for (id, _) in OPERATION_OVERRIDES {
            assert!(
                known.contains(&id.to_string()),
                "classification override references unknown operationId '{id}'"
            );
        }
    }

    #[test]
    fn every_spec_operation_is_internally_consistent() {
        let spec: Value = serde_json::from_str(SPEC).expect("fixture spec is valid JSON");
        let mut checked = 0;
        for (path, methods) in spec["paths"].as_object().expect("paths") {
            for (method, op) in methods.as_object().expect("methods") {
                if !is_http_method(method) {
                    continue;
                }
                let id = crate::openapi::operation_id(method, path, op);
                assert!(
                    for_operation(method, &id, op).is_ok(),
                    "{method} {path} ({id}) is both read-only and destructive"
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "fixture spec produced no operations");
    }

    /// The confirmation policy, pinned.
    ///
    /// The spec carries no classification metadata, so every `POST`/`PUT` takes
    /// the method default unless [`OPERATION_OVERRIDES`] names it. This test
    /// states the resulting set exactly: a write that starts requiring `--yes`,
    /// or quietly stops requiring it, has to change this list to land.
    #[test]
    fn only_the_reviewed_writes_require_confirmation() {
        const EXPECTED: &[&str] = &[
            "post-alerts-id-actions",
            "post-service-outages-overrides",
            "put-service-outages-overrides-id",
            "put-services-id-private-subscribers",
            "put-status-pages-id-private-subscribers",
            "put-escalation-policies-id-levels-level",
        ];

        let spec: Value = serde_json::from_str(SPEC).expect("fixture spec is valid JSON");
        let mut destructive = Vec::new();
        let mut writes = 0;

        for (id, method, path) in operations(&spec) {
            if method != "POST" && method != "PUT" {
                continue;
            }
            writes += 1;
            let op = &spec["paths"][&path][method.to_lowercase()];
            if for_operation(&method, &id, op).unwrap().destructive {
                destructive.push(id);
            }
        }

        assert!(writes > 0, "fixture spec produced no POST/PUT operations");

        destructive.sort();
        let mut expected: Vec<String> = EXPECTED.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(
            destructive, expected,
            "the set of writes requiring --yes changed; review the new operation against the \
             confirmation policy in this module's docs before updating this list"
        );
    }

    /// The other half of the policy: a `POST` that only queries is not a write.
    #[test]
    fn a_post_that_only_queries_is_read_only() {
        let spec: Value = serde_json::from_str(SPEC).expect("fixture spec is valid JSON");
        for (id, path) in [
            ("post-incidents-publish-info", "/incidents/publish-info"),
            ("post-users-search-email", "/users/search-email"),
        ] {
            let op = &spec["paths"][path]["post"];
            let c = for_operation("POST", id, op).unwrap();
            assert!(c.read_only, "{id} should be read-only");
            assert!(!c.destructive, "{id} should not require confirmation");
        }
    }

    #[test]
    fn every_static_command_is_internally_consistent() {
        for (name, c) in STATIC_COMMANDS {
            assert!(
                c.is_consistent(),
                "static command '{name}' is both read-only and destructive"
            );
        }
    }

    /// The gate is only as good as its coverage: a static command added without
    /// a classification would never be considered destructive, whatever it does.
    #[test]
    fn every_static_command_is_classified() {
        for path in crate::cli::static_command_paths() {
            if PER_INVOCATION_COMMANDS.contains(&path.as_str()) {
                continue;
            }
            assert!(
                for_static_command(&path).is_some(),
                "static command '{path}' has no classification — add it to STATIC_COMMANDS \
                 (or to PER_INVOCATION_COMMANDS if it resolves one per invocation)"
            );
        }
    }

    /// The reverse direction: an entry that no longer names a real command is a
    /// classification nobody consults, and hides the fact that the real command
    /// has none.
    #[test]
    fn no_classification_names_a_command_that_does_not_exist() {
        let paths = crate::cli::static_command_paths();
        for (name, _) in STATIC_COMMANDS {
            // The `alerts *` aliases hang off a dynamic tag, so they are not in
            // the static tree.
            if name.starts_with("alerts ") {
                continue;
            }
            assert!(
                paths.iter().any(|p| p == name),
                "STATIC_COMMANDS names '{name}', which is not a command in the CLI tree"
            );
        }
    }

    #[test]
    fn static_command_names_are_unique() {
        let mut seen: Vec<&str> = STATIC_COMMANDS.iter().map(|(n, _)| *n).collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            before,
            seen.len(),
            "duplicate static command classification"
        );
    }

    fn is_http_method(m: &str) -> bool {
        matches!(m, "get" | "head" | "post" | "put" | "patch" | "delete")
    }

    /// Every operation in the spec as `(id, method, path)`, resolved through the
    /// same helper the index uses — the ilert spec declares no `operationId`, so
    /// reimplementing the fallback here would test a key nothing else consults.
    fn operations(spec: &Value) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for (path, methods) in spec["paths"].as_object().expect("paths") {
            for (method, op) in methods.as_object().expect("methods") {
                if !is_http_method(method) {
                    continue;
                }
                out.push((
                    crate::openapi::operation_id(method, path, op),
                    method.to_uppercase(),
                    path.clone(),
                ));
            }
        }
        out
    }

    fn operation_ids(spec: &Value) -> Vec<String> {
        operations(spec).into_iter().map(|(id, _, _)| id).collect()
    }
}
