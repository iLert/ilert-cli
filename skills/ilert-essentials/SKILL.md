---
name: ilert-essentials
description: Core ilert concepts and CLI behaviour: the object model and the rules behind the field names
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
invisible to people who could see them before. The suggested default is to keep
teams `PUBLIC` and scope write access rather than visibility.

**Status is not communication.** Setting a service's status notifies nobody;
subscribers hear a **Status update** naming that service. That split is what lets
you fix a wrong status without mailing your customers.

**Prefer the async event API for machine traffic.** `POST /api/events` with
`eventType` `ALERT` / `ACCEPT` / `RESOLVE` / `COMMENT`. The synchronous
`/api/alerts/{id}` verb endpoints exist and are fine for human-driven or
one-off work. It authenticates with an **integration key, not a bearer token**,
and answers **`202` with an empty body**: queued, not created. No alert id, no
dedup verdict, and a payload the source filtered or rejected still returns `202` —
the reason lands in the alert source's event log, never in the response. To learn
what happened, poll `/alerts` by alert key.

## Talking to the API, not just the CLI

Relevant when you use `ilert api` or read raw responses.

**The spec documents the success path.** It does not describe `400`, `401`, `402`
or `429` responses, and its `RestError` schema omits `code` and `detailedCode`, so
error handling generated from the spec will not match the wire. Model every non-2xx
as `{status, message, code, details?, detailedCode?}` and branch on `code`.

A missing `Authorization` header is `401`; an unknown, revoked or malformed **API key is `403`** with
`code: KEY_ERROR`, as is a missing OAuth2 scope (`OAUTH2_SCOPE_ERROR`, plus a
`scope` field). Re-auth logic keyed on `401` never fires for a revoked key, but `403`.

**`429` carries no `Retry-After` and no rate-limit headers.** There are two
independent buckets — REST calls per token, and events per integration key — both
counted in fixed one-minute windows. The defaults are on the order of ~120 REST
calls and ~50 events per minute, but **treat those as a rough scale, not a
contract**: individual accounts and integrations can be limited differently, so
derive your pacing from the `429`s you actually get rather than from a number
hard-coded against the default.

**Page caps are per endpoint, and overshoot is a hard `400`, not a clamp.** They
differ widely — tens on some collections, a couple of hundred on others — and
`?include=` lowers the cap further sometimes on the endpoints that support it. Read the cap
off `max-results`' `maximum` in the spec for the operation you are calling rather
than assuming one number everywhere; that is what the CLI does. Paging is
offset-based: there is no total count, no next-page link, and usually no sort
parameter, with ordering server-defined — so, as with any offset scheme, paging a
collection that changes underneath you can skip or repeat rows.
`/heartbeat-monitors` pages by `cursor` rather than `start-index`.

**`include` is opt-in and there is no `PATCH`.** `integrationKey`,
`escalationRules`, `customDetails`, alert-source templates, alert-action
`conditions` and schedule `shifts` are omitted unless you ask for them via
`?include=`. `PUT` is the only update verb and is a **full replace** — so "GET it,
change one field, PUT it back" drops everything the GET did not return. Most
`include` fields are sub-dependencies or read-only. Build the PUT body explicitly.

**`null` does not always mean "not configured".** `integrationKey`,
`integrationUrl` come back `null` for callers without
update permission, on an ordinary `200`. Read a `null` there as "not
available to this key", and do not conclude the field is unset.

**`404` means the entity is not there for you.** Usually that is the obvious
thing: it does not exist. It also covers entities outside what your key can see —
the API deliberately does not distinguish the two, so that a `404` cannot be used
to confirm that something exists. Do not try to read a permission verdict out of
it; widen the key's scope or the team context and ask again.

**Some standard enums in the spec are illustrative.** `TimeZone` lists four zones while the
server takes any IANA id; integration, connector and priority types grow most
releases. Deprecation is noted in prose field descriptions rather than through the
OpenAPI `deprecated` flag. Treat enums as open, and ignore unknown JSON properties
— the compatibility promise is that fields get added, never removed.

**Timestamps are UTC with variable precision.** `…T10:00:00Z`,
`…T10:00:00.123Z` and `…T10:00:00.123456Z` all occur, so parse with a real
ISO-8601 parser, never a fixed pattern. Input is lenient and accepts offsets, but
nothing echoes your offset back.

**If it is not in `openapi.json`, do not build on it.** The spec is the supported
public surface; anything else you may find reachable is internal or in alpha, and
may change without notice.

## Driving the CLI

### Mode decides whether you get asked

The CLI resolves a mode before anything else (`ILERT_CLI_MODE` → agent env marker
→ CI env marker → TTY → `ci` as the conservative fallback). **Only `interactive`
can prompt.** In `agent` or `ci` mode a destructive command does not ask and does
not proceed — it exits **`2`** with a JSON envelope on stderr.

Exit `2` means *"nobody consented"*, not *"it failed"* (that is exit `1`).
Retrying will not help; pass `--yes`, or decide not to.

### Plan limits arrive as errors, not as empty results

Treat **`402`** as the plan signal and branch on the body's `code`:
`FEATURE_REQUIRED` (the plan does not include it — no amount of retrying or
rephrasing helps), `QUOTA_EXCEEDED` (included, but used up) or `ERROR`. Read
`detailedCode` when it is there — it names the feature — but do not require it.
`QUOTA_EXCEEDED` puts its numbers in the message prose (`"Limit: 5, usage: 5."`), not in fields.

Do not treat `402` as the only case: some (older) plan errors arrive as **`400` or `404`**
with an upgrade-your-plan message in the body instead. Check the message before concluding the
resource does not exist.

None of these are retried — the retry list is `429` and `5xx` only — and the CLI
surfaces the body's `message` (or `error`) as the error text while keeping the
full response for `--debug`.

Do not attempt to query features first — react to the error. When you need to know which tier includes a feature,
that lives at <https://www.ilert.com/pricing>.

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

Pagination caps at **200 pages**. Past that it warns on **stderr** and returns
what it has, as a perfectly valid JSON array. Capture stdout only and a short
export looks complete. Raise `--max-results`, or narrow with `--from` / `--until`
and other filters.

Page size is the smaller of `--max-results` and the endpoint's own ceiling, so
`--all` fetches 20 at a time on `/schedules` and 50 on `/alerts` without being
told; asking for more than an endpoint allows warns and clamps rather than
failing. `--all` is only offered on operations that actually page by offset — a
collection that returns everything at once, or `heartbeat-monitors`, which pages
by cursor, does not have the flag.

### One envelope, two situations

`--dry-run` and a refusal emit the same JSON shape — `status`, `operation`,
`classification` (`read_only` / `destructive` / `idempotent`), `request`,
`confirmation` — differing only in `status` (`dry_run` vs `confirmation_required`)
and stream (stdout vs stderr). So: dry-run first, parse one shape.

A failure is a third shape, and in every JSON mode it carries the API's own
fields: `{"error": {message, status, code?, detailedCode?, details?}}`. So branch
on `error.code` — `FEATURE_REQUIRED`, `KEY_ERROR` — instead of matching on the
message text. `details` is the API's error body verbatim, and is omitted when the
response was not a JSON object (a gateway or a wrong path answers with an HTML
page, which is not worth relaying).

There is deliberately no reconstructed `confirmCommand`; you get the name of the
flag that grants consent and nothing that could echo back a credential. Sensitive
headers, and any body field whose normalized name contains `password`, `secret`,
`token`, `apikey`, `credential`, `signature` — and note **`integrationkey` and
`routingkey`** — are redacted from previews and `--debug` alike. So a dry run of
an event send will not show you the key it would use; that is intended, not a bug
to work around.

### Before you run a batch

* **Requests are retried** up to 3 times on `429`, `502`, `503`, `504` and network
  errors — for every method, `POST` included. Send an `alertKey` so a retried
  event groups instead of duplicating. Backoff starts at 500 ms, except on `429`,
  where it starts at 5 s (5 → 10 → 20) to outlast the server's fixed one-minute
  window; a rate-limited command therefore pauses and says so
  on stderr.
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
  that scopes what you see — `0` all teams, `-1` my teams, or a team id. An easy
  explanation for "the list came back short".
