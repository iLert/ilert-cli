---
name: ilert-essentials
description: How ilert and the ilert CLI actually behave — the rules that are not in --help or the field names
user-invocable: true
---

# ilert essentials

`ilert skills list` is the index of everything else — including the
`migrate-from-*` playbooks. Migrating an existing setup? Read this first, then
the relevant one: they cover the mapping, this covers the platform.

## The object model in one pass

| Object | What it is |
| --- | --- |
| **Event** | What monitoring tools send. `integrationKey` routes it to an alert source |
| **Alert Source** | Receives events. Owns the `escalationPolicy`, the `integrationKey` / `integrationUrl`, and every parsing and grouping setting |
| **Alert** | The actionable thing: `PENDING` → `ACCEPTED` → `RESOLVED`, created by an alert source, driven by an escalation policy, triggers notifications to responders |
| **Service** | A business capability people subscribe to. Appears on status pages, carries outage history, is attached to sources and named on incidents. Owns no policy and receives no events |
| **Incident** | The coordination record above alerts (used on business impact) — multi channel responders, timeline, incident channel. It publishes nothing by itself |
| **Status update** | The public message posted *from* an incident. This is what reaches status pages and subscribers |
| **Escalation Policy** | Ordered rules with `escalationTimeout`, targeting users, schedules or teams. Also `repeating` / `frequency`, `delayMin`, `routingKey` |
| **Schedule** | `STATIC` or `RECURRING`; `RECURRING` carries `scheduleLayers`. Overrides go through `PUT /schedules/{id}/overrides` |
| **Event Flow** | A routing layer *above* alert sources with its own ingest URL. Nodes: `ROOT`, `DEFINE_BRANCHES`, `ROUTE_EVENT`, `SUPPORT_HOURS`, `WAIT`, `TRANSFORM` |
| **Call Flow** | The same idea for inbound phone calls: `IVR_MENU`, `AUDIO_MESSAGE`, `SUPPORT_HOURS`, `ROUTE_CALL`, `PARALLEL_ROUTE_CALL`, `VOICEMAIL`, `PIN_CODE`, `CREATE_ALERT`, `BLOCK_NUMBERS`, `AGENTIC` |
| **Alert Action + Connector** | Outbound automation. The Connector holds credentials, the Alert Action binds it to a source with `triggerMode`, `triggerTypes` and ICL `conditions` |
| **Maintenance Window** | Takes both `services` *and* `alertSources` — communication and suppression in one object |
| **Deployment Pipeline / Event** | Deploy traffic, on its own ingest endpoint and its own integration key. Not alert traffic |

> Node and enum lists grow; check the api-docs rather than treating these as closed.

## Behaviour that is not in the field names

**Priority and severity are different fields.** `priority` is `HIGH` / `LOW` and
drives notification and escalation. `severity` is an **integer 1–5** on the event
(rendered `SEV1`–`SEV5`; sending the string `"SEV1"` is a validation error) and
carries impact. Both can be derived on the source by `priorityTemplate` and
`severityTemplate` instead of being sent per event.

**`alertCreation` decides how many alerts an event stream produces** —
`ONE_ALERT_PER_EMAIL` (default, "every event opens a new alert"),
`ONE_ALERT_PER_EMAIL_SUBJECT` (per new event summary), `ONE_PENDING_ALERT_ALLOWED`,
`ONE_OPEN_ALERT_ALLOWED`, `OPEN_RESOLVE_ON_EXTRACTION`,
`ONE_ALERT_GROUPED_PER_WINDOW` and `INTELLIGENT_GROUPING` (the last two consult
`alertGroupingWindow`). **But an `alertKey` matching an open alert groups
regardless**, and with a dedicated `integrationType` — not `API`, e.g.
`PROMETHEUS` — ilert extracts alert keys automatically. Often overlooked, and
usually what you wanted. The key is trimmed and compared **case-insensitively**,
so `SRV-1` and `srv-1` are the same alert.

**`ACCEPTED` stops escalation, by design.** Accepting means a human owns it, so
nobody gets re-paged. For a "still not resolved" safety net use an Alert Action on
the `v-alert-not-resolved` trigger rather than escalation.

**Support hours downgrade, they do not suppress.** `alertPriorityRule` with
`HIGH_DURING_SUPPORT_HOURS` creates `LOW` alerts outside those hours; it does not
withhold them. A `LOW` alert keeps only the **first** escalation rule and never
advances past it. `autoRaiseAlerts` re-raises still-`PENDING` alerts when support
hours begin. Real quiet lives in the recipients' notification preferences, or in a
flow branch that drops the event.

**Escalation to an empty schedule falls through instantly**, without waiting for
the escalation timeout. That is how you chain several schedules in a complex
on-call setup — and why a policy imported before its schedules looks correct and
pages nobody.

**An event flow drops by absence.** A path that never reaches `ROUTE_EVENT` is
handed to no alert source and creates no alert. It stays visible in the flow's
logs, but a deliberate stop and an unfinished branch look identical in the tree.

**For simple routing, don't reach for a flow.** An Escalation Policy carries a
`routingKey`, and an `ALERT` event reaches it either by sending `routingKey`
directly or by having the source's `routingTemplate` pull it out of the payload —
comma-separated, evaluated left to right, falling back to the source's own policy
when none match. Use a flow when you need conditions, support hours, transforms or
a tree.

**Assigning to a team does not restrict anything.** Only `visibility: PRIVATE`
narrows access, and a user who joins a private team becomes a private user,
invisible to people who could see them before. Public teams are the suggested best
practice to reduce MTTR.

**Status is not communication.** Setting a service's status notifies nobody;
subscribers hear a **Status update** naming that service. That split is what lets
you fix a wrong status without mailing your customers.

**Prefer the async event API for machine traffic.** `POST /api/events` with
`eventType` `ALERT` / `ACCEPT` / `RESOLVE` / `COMMENT`. The synchronous
`/api/alerts/{id}` verb endpoints exist and are fine for human-driven or
one-off work.

## Driving the CLI

### Mode decides whether you get asked

The CLI resolves a mode before anything else (`ILERT_CLI_MODE` → agent env marker
→ CI env marker → TTY → `ci` as the conservative fallback). **Only `interactive`
can prompt.** In `agent` or `ci` mode a destructive command does not ask and does
not proceed — it exits **`2`** with a JSON envelope on stderr.

Exit `2` means *"nobody consented"*, not *"it failed"* (that is exit `1`).
Retrying will not help; pass `--yes`, or decide not to.

### What the CLI treats as destructive

A write is **not** destructive by default. `DELETE` is, plus a specific list that
no HTTP method would tell you:

| Operation | Why |
| --- | --- |
| `POST /alerts/{id}/actions` | Fires the webhook, opens the ticket, posts to chat. Nothing unsends it |
| `POST` / `PUT /service-outages/overrides` | Rewrites public uptime history |
| `PUT /services/{id}/private-subscribers` | *Set* semantics — everyone omitted is removed |
| `PUT /status-pages/{id}/private-subscribers` | Same. The sibling `POST` *adds* and is not destructive |
| `PUT /escalation-policies/{id}/levels/{level}` | Replaces the level, dropping whoever held it, on a paging path |

In reverse, two `POST`s are classified **read-only** because they only query:
`POST /incidents/publish-info` (which subscribers an incident would reach) and
`POST /users/search-email`.

Creating an incident or sending an event is outward-facing and irreversible too,
but is not gated — that is the documented purpose of those commands, and a prompt
on every write trains people to pass `--yes` unconditionally.

### `--all` stops early and only warns on stderr

Pagination caps at **200 pages** — 10,000 items at the default `--max-results` of
50. Past that it warns on **stderr** and returns what it has, as a perfectly valid
JSON array. Capture stdout only and a short export looks complete. Raise
`--max-results`, or narrow with `--from` / `--until` and other filters.

### One envelope, two situations

`--dry-run` and a refusal emit the same JSON shape — `status`, `operation`,
`classification` (`read_only` / `destructive` / `idempotent`), `request`,
`confirmation` — differing only in `status` (`dry_run` vs `confirmation_required`)
and stream (stdout vs stderr). So: dry-run first, parse one shape.

There is deliberately no reconstructed `confirmCommand`; you get the name of the
flag that grants consent and nothing that could echo back a credential. Sensitive
headers, and any body field whose normalized name contains `password`, `secret`,
`token`, `apikey`, `credential`, `signature` — and note **`integrationkey` and
`routingkey`** — are redacted from previews and `--debug` alike. So a dry run of
an event send will not show you the key it would use; that is intended, not a bug
to work around.

### Before you run a batch

* **Requests are retried** up to 3 times with exponential backoff from 500 ms, on
  `429`, `502`, `503`, `504` and network errors — for every method, `POST`
  included. Send an `alertKey` so a retried event groups instead of duplicating.
* **`--stdin` is one request per line** against the template on the command line,
  varying only the ID. Consent is answered **once** for the whole batch.
* **Path parameters must be a single segment.** `/`, `\`, `.`, `..` and control
  characters are refused; anything else is percent-encoded, so a pre-encoded
  `%2F` becomes a literal and 404s rather than re-targeting the request.
* **The spec is cached for 24 hours**, so the command tree can lag a fresh API
  change. `ilert ops list` shows raw operations and `ilert api /any/path` bypasses
  the generated tree.
* **`ILERT_API_KEY` beats the keyring** and is never persisted. `--api-key` beats
  both. `ILERT_TEAM_CONTEXT` (or `--team-context`) adds an `x-team-context` header
  that scopes what you see — an easy explanation for "the list came back short".
