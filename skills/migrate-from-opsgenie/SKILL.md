---
name: migrate-from-opsgenie
description: Map Opsgenie resources onto ilert equivalents
user-invocable: true
---

# Migrating from Opsgenie to ilert

The hard part is not the object mapping — it is that the two products route
alerts through different objects. Opsgenie routes **team-first**; ilert routes
**source-first**. A simple setup maps one-to-one, but any configuration that
leaned on team routing rules routes differently after a faithful field-by-field import.

For ilert's own semantics and the CLI behaviour behind a bulk import, read the
`ilert-essentials` skill alongside this one. Where Opsgenie's own data lives — and
how to hold a foreign API key while you read it — is under *Reading the Opsgenie
side* below; start there when the job is extraction rather than design.

## The routing model difference

In Opsgenie an integration hands an alert to a **team**, and the team's routing
rules decide which escalation applies, with alert/notification policies filtering
along the way.

In ilert the **Alert Source** owns the escalation policy directly. There is no
object equivalent to a team routing rule, but two mechanisms between them absorb
almost every routing rule you will find, and they are where to look first:

* **Routing keys — when the emitter can name the branch it wants.** An Escalation
  Policy carries a `routingKey`. An event supplying a matching key overrides the
  alert source's own policy. Keys are comma-separated and evaluated left to
  right, falling back to the source's policy when none match, and the key can be
  pulled from the payload with the alert source's `routingTemplate`. One alert
  source, several escalation paths, no extra objects.

* **Event Flows — for everything else.** This is the general answer whenever an
  Opsgenie rule has no direct equivalent in an alert source setting. An Event Flow is a routing layer
  that sits *above* alert sources rather than beside them: it has its own
  `integrationKey` / `integrationUrl`, so emitters post to the **flow** instead
  of to a source, as a drop-in replacement for a source URL at the sender. Inside
  it is a tree of nodes:

  | Node | What it does                                                                                                                                                                  |
  | --- |-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
  | `DEFINE_BRANCHES` | Conditions over the event body (`context.event.summary`, `context.event.customDetails.…`), AND/OR groups, evaluated in order — first match wins, with a `CATCH_ALL` else path |
  | `ROUTE_EVENT` | Hands the event to an alert source, optionally overriding that source's `escalationPolicyId` and `overwritePriority` for this branch                                          |
  | `SUPPORT_HOURS` | Branches on whether the event arrived inside a support-hours window                                                                                                           |
  | `WAIT` | Holds the event before the next node runs, also capable of dropping delayed events when a following ACCEPT/RESOLVE arrives with a matching alertKey                           |
  | `TRANSFORM` | Rewrites fields before routing (`SET`, `COPY`, `MAP`, `TEMPLATE`, `MERGE`, `APPEND_ARRAY`)                                                                                    |

   > Other nodes might be available based on api-docs

  `ROUTE_EVENT` is the piece that does the real work: it lets **one** alert source
  serve many escalation paths, chosen per branch, which is exactly what an
  Opsgenie team routing rule did. `SUPPORT_HOURS` covers the time-based branches
  the emitter could never have signaled, and `TRANSFORM` is where an Opsgenie
  *alert policy* lands. (but remember: for simple routings `routingKey` on the alert source is the best approach and an event flow is overkill).

Reach for **additional alert sources** only when branches genuinely need
different *source-level* settings — a different `integrationType`,
`alertCreation` mode or `autoResolutionTimeout`. Those live on the source and a
flow cannot override them; `ROUTE_EVENT` overrides escalation policy and priority
and nothing else. That is the real dividing line, and it is a much narrower
reason to multiply sources than "the routing differs".

Teams in ilert carry no routing rules, but they are not purely decorative: an
escalation rule can target a team directly, so teams do appear in notification
paths.

Flatten the routing rules first, on paper, before creating anything. The usual
landing place is one Event Flow plus a handful of alert sources — not one source
per rule. The count of alert sources is a migration decision, not a translation.

## Resource mapping

| Opsgenie | ilert | Notes                                                                                                                                     |
| --- | --- |-------------------------------------------------------------------------------------------------------------------------------------------|
| Integration | Alert Source | Carries `escalationPolicy`, `integrationKey`, `integrationUrl`                                                                            |
| API key / integration key | Alert Source `integrationKey` | New value; every emitter must be re-pointed                                                                                               |
| Team | Team | Visibility and ownership; carries no routing rules, but can be an escalation rule target                                                  |
| Team routing rule | Event Flow, or Escalation Policy `routingKey` | No single equivalent, but an Event Flow covers nearly all of it — see above                                                               |
| Escalation | Escalation Policy | Rules hold `escalationTimeout` plus `users` / `schedules` / `teams`                                                                       |
| Schedule | Schedule | `type` is `STATIC` or `RECURRING`                                                                                                         |
| Rotation | `scheduleLayers` | On `RECURRING` schedules                                                                                                                  |
| Override | Shift, via `PUT /schedules/{id}/overrides` | Same `Shift` payload as a schedule shift, but its own endpoint; must not be in the past                                                   |
| Alert | Alert | `PENDING` → `ACCEPTED` → `RESOLVED`                                                                                                       |
| Alert `alias` | Alert `alertKey` | Deduplication identity                                                                                                                    |
| Incident (Incident Management) | Incident | Internal coordination on both sides: responders who are paged, associated/linked alerts, notes and timeline                               |
| Incident `responders` | Incident responders | ilert pages via escalation policy, schedule, individual or whole team                                                                     |
| Incident `statusPageEntry` | Status update | The public half of an Opsgenie incident is a separate object in ilert, with its own `/status-updates` endpoint — see below                |
| Incident rule (auto-create from alert) | `ilert incidents` Alert Action on the alert source | Automatic and conditional — see below; incidents can also be declared by a caller from the UI, API or MCP                                 |
| Service | Service | Not only a status-page component: also attached to alert sources (`services`, `autoCreateServices`) and to incidents as affected services |
| Status page | Status Page | Page metrics map to `Metric` + `Metric Data Source`; see the `migrate-from-statuspageio` skill for the page-side detail                   |
| Incident template | Incident Template | `sendNotification` on the template decides whether subscribers are notified                                                               |
| *(no equivalent)* | Deployment Event + Deployment Pipeline | Capability gained, not migrated — see below                                                                                               |
| User | User |                                                                                                                                           |
| Contact method | Contact | Separate objects per channel                                                                                                              |
| Notification rule | Notification Preference | Split by priority *and* by notification type                                                                                              |
| Alert policy (`modify`) | Event Flow `TRANSFORM` node, or `ROUTE_EVENT` `overwritePriority` | Field rewriting and priority overrides                                                                                                    |
| Notification policy (auto-close, dedup) | Alert Source `autoResolutionTimeout`, `alertCreation`, `alertGroupingWindow` | Source-level only; a flow cannot override these                                                                                           |
| Notification policy (delay) | Escalation Policy `delayMin` | Delayed escalation — a third place; see below                                                                                             |
| Maintenance | Maintenance Window |                                                                                                                                           |
| Heartbeat | Heartbeat Monitor |                                                                                                                                           |
| Incoming call routing | Call Flow + Call Flow Number | A whole product area, easily forgotten — see below                                                                                        |
| Forwarding rule | Schedule override, per schedule | Narrower than Opsgenie's, and not a single object — see below                                                                             |
| Custom user role | `Role` / `TeamRole` | Fixed enums (`ADMIN`, `USER`, `RESPONDER`, `STAKEHOLDER`, `GUEST`), (custom rbac roles require Enterprise plan)                           |
| Team member + team role | `/teams/{id}/members` with `TeamRole` |                                                                                                                                           |
| Who is on call | `/on-calls` | Not a migrated object; the endpoint to verify coverage after import                                                                       |
| Action / outgoing webhook | Alert Action + Connector | Connector holds credentials, Alert Action the binding                                                                                     |
| Priority P1–P5 | Alert severity, integer `1`–`5` on the event (displayed `SEV1`–`SEV5`) | One for one — see below; priority `HIGH`/`LOW` is a separate, two-valued field                                                            |
| Escalation rule `delay.timeAmount` | Escalation rule `escalationTimeout` | Per rule on both sides; minutes on both sides                                                                                             |
| Escalation `repeat` | Escalation Policy `repeating` / `frequency` | Moves from the escalation to the policy — see below                                                                                       |

## Mappings that silently change behaviour

**Priority does not collapse — severity carries the level.** Opsgenie's single
P1–P5 field splits across two ilert fields, and both are worth setting.
**Severity** is a five-level scale, so P1–P5 maps one for one and nothing is
lost. On the wire it is an **integer 1–5** (`1` most severe), rendered
`SEV1`–`SEV5` in the UI — sending `"P1"` or `"SEV1"` is a validation error, so map
numerically. Set it per event with `severity`, or let the alert source's
`severityTemplate` derive it from the payload; the event value overwrites the
template's. **Priority** is separately two-valued (`HIGH` / `LOW`) and is
what actually drives notification and escalation behaviour, so it needs its own
decision: a common, defensible split is P1/P2 → `HIGH` and P3–P5 → `LOW`, set via
`priorityTemplate` or `alertPriorityRule`. Confirm that split against how
notification rules were actually written in Opsgenie, not against the label
names — and note that it is now a routing decision only, not a lossy one, because
the original level survives in severity. Incidents use the same `SEV1`–`SEV5`
scale and default to `SEV3`.

**Close and acknowledge are renamed, not redefined.** Opsgenie's *close* becomes
ilert's *resolve*; *acknowledge* becomes *accept*. Both products allow closing or
resolving an alert that was never acknowledged, and both keep the alert in
history afterwards — the lifecycle is the same shape, so do not budget for a
behavioural change here. What does break is anything reading the values:
automation counting "acknowledged then closed" transitions has to be re-pointed
at `ACCEPTED` and `RESOLVED`, and any report keyed on Opsgenie's five priorities
has to read ilert's `severity`, not its `priority`.

**Alert policies and notification policies split across three objects.** Opsgenie
keeps de-duplication, auto-close and delay in *notification policies*, and field
rewriting in *alert policies*. In ilert those land in three different places:
de-duplication and auto-close are alert source settings (`alertCreation`,
`alertGroupingWindow`, `autoResolutionTimeout`), rewriting is an Event Flow
`TRANSFORM` node, and
**delay is `delayMin` on the Escalation Policy** — ilert's *delayed escalation*,
which holds an alert for a set period and notifies nobody if it resolves inside the window. That last one is the natural home for an
Opsgenie policy that deferred notification on flapping monitors, and it is easy
to miss because it is the one piece that lives on the policy rather than on the
source. An alert that self-resolves during the delay pages no one, but you can
still record it — an Alert Action triggered on `alert-created` fires regardless.

Each Opsgenie policy therefore scatters across objects. Reviewing only one of the
three loses behaviour silently.

The two also differ in scope, which decides what you can even find: Opsgenie
notification policies exist only inside *team* policies, while alert policies
exist both globally and per team. A global alert policy belongs to no team and is
easy to miss entirely when migrating team by team.

**Deduplication is governed by mode, not just by key.** `alias` → `alertKey` is
the easy half. What decides how many alerts you actually get is the alert
source's `alertCreation` mode — it is the grouping rule, not a flag:

| Mode | Opens a new alert                                                |
| --- |------------------------------------------------------------------|
| `ONE_ALERT_PER_EMAIL` | for every event                                                  |
| `ONE_ALERT_PER_EMAIL_SUBJECT` | for every new event summary                                      |
| `ONE_PENDING_ALERT_ALLOWED` | unless one is already `PENDING`                                  |
| `ONE_OPEN_ALERT_ALLOWED` | unless one is already open — `PENDING` or `ACCEPTED`             |
| `OPEN_RESOLVE_ON_EXTRACTION` | per alert key extracted from the payload, which also resolves it |
| `ONE_ALERT_GROUPED_PER_WINDOW` | if the last one is older than `alertGroupingWindow`              |
| `INTELLIGENT_GROUPING` | if the last *similar* one is older than `alertGroupingWindow`    |

Opsgenie expresses the same intent through a
`notification-deduplication` policy that is value- or frequency-based, so there
is no mode-for-mode correspondence to copy across. Decide per source what "the
same alert" should mean and set the mode explicitly — the default is
`ONE_ALERT_PER_EMAIL`, an event-shaped answer that means "every event opens a new
alert". However, when an alertKey is present and matching an open alert it will be grouped regardless,
using a dedicated integrationType  (not API e.g. PROMETHEUS) ilert will automatically extract alertKeys based
on best practices - this is an often overlooked behavior.

**Support hours downgrade, they do not suppress.** `alertPriorityRule` takes
`HIGH`, `LOW`, `HIGH_DURING_SUPPORT_HOURS` or `LOW_DURING_SUPPORT_HOURS`. Setting
`HIGH_DURING_SUPPORT_HOURS` makes alerts `LOW` outside those hours — they are
still created and still notify according to each user's `LOW` preferences. The
larger change is that a `LOW` alert keeps only the **first** escalation rule: the
level-one target is notified, and the alert never advances past it no matter how
long it goes unaccepted. Opsgenie configurations that used time restrictions or a
`notification-suppress` policy to mean "do not page" therefore cannot express the
quiet part through support hours alone.

There are two better homes for it, and which one you want depends on whether the
alert should exist at all. If it should — visible, just not paging anyone — the
quiet belongs in the recipients' **notification preferences**. If it genuinely
should not exist, express it in an **Event Flow**: an event whose path never
reaches a `ROUTE_EVENT` node is handed to no alert source and creates no alert,
which is a real drop, and a `SUPPORT_HOURS` node in front of it makes that drop
time-based. That is usually the closer match for a suppression policy, and unlike
notification preferences it is configured centrally rather than per user — the
event still shows in the flow's logs, so it stays auditable.

Related, and easy to leave off: `autoRaiseAlerts` re-raises alerts still
`PENDING` when support hours begin. An Opsgenie setup that deliberately deferred
out-of-hours work to the morning is usually expressing exactly this.

**An Opsgenie incident splits into two ilert objects.** `/incidents` is the
**Incident** — the coordination record, with responders, paging, linked alerts,
timeline and incident channel. `/status-updates` is the **Status update**, the
public message posted from an incident, carrying `affectedServices` and the
statuses `INVESTIGATING` / `IDENTIFIED` / `MONITORING` / `RESOLVED`. An Opsgenie
incident's `statusPageEntry` becomes the second; everything else about it —
responders, notes, impacted services — becomes the first. Migrating an Opsgenie
incident to a status update alone silently drops the response side.

Keep this in proportion. Opsgenie incident *records* are historical data, and
historical alerts and incidents might not need to migrate anyway — so this mapping matters
for understanding the two models and for anything that creates incidents going
forward, not for a bulk import.

Opsgenie's incident rules — open an incident automatically when alert data
matches a condition — do translate. The equivalent is an **Alert Action** of type
`ilert incidents` attached to the alert source, with `triggerMode` set to
`AUTOMATIC`: it generates an incident and drives service status and status
updates as alerts arrive, without a human in the loop. The status update it posts
is built from an **Incident Template**, whose `sendNotification` flag decides
whether subscribers are actually notified — set it deliberately, because an
automated update that quietly pages a subscriber list is a very different thing
from one that only repaints the page. The conditional half is
`conditions`, an ICL expression evaluated against the alert, narrowed further by
`triggerTypes` (`alert-created`, `alert-acknowledged`, `alert-escalation-ended`,
…). Leave `triggerMode` on `MANUAL` and the same action becomes a button on the
alert instead.

What changes is where the rule lives and what it can see. Opsgenie evaluates
incident rules centrally, per team; ilert attaches the action per alert source
and evaluates it against that alert's own fields. So a single Opsgenie rule
spanning several integrations becomes one action per alert source, and any
condition that referenced state outside the alert has to be re-expressed in alert
terms or left to a manual declare. Rebuild the rules — do not budget for losing
them.

**Forwarding rules have no global equivalent.** An Opsgenie forwarding rule
redirects *everything* aimed at one user to another for a date range — whether
that user was reached through a schedule, through an escalation rule, or by
direct assignment. ilert mitigates the necessity for this, by providing bulk schedule-override
options in My-on-calls or the coverage-request feature.

**Call routing is its own migration.** Opsgenie's incoming call routing — a phone
number that routes callers to on-call staff, with auto-attendant menus, voicemail
and blocklists — maps onto ilert **Call Flows**: a tree of nodes (`IVR_MENU`,
`AUDIO_MESSAGE`, `SUPPORT_HOURS`, `ROUTE_CALL`, `PARALLEL_ROUTE_CALL`,
`VOICEMAIL`, `PIN_CODE`, `CREATE_ALERT`, `BLOCK_NUMBERS`, and more check the api-docs) attached to a call flow
number. The node structure transfers, but **the routing targets do not line up exactly**:
Opsgenie routes a call to a user, schedule, team or escalation policy, while an
ilert `ROUTE_CALL` node targets `USER`, `ON_CALL_SCHEDULE`, `TEAM` or `NUMBER` (enterprise feature). Any
route that pointed to an escalation policy has to be expanded or moved into a team — usually
as the schedule behind it, which silently drops the escalation fallback when
nobody answers. `callStyle` (`ORDERED`, `RANDOM`, `PARALLEL`), `retries` and
`callTimeoutSec` on the node are where you rebuild part of that behaviour. Note
that this choice is made in all clarity as policies contain escalation timeouts
and a live incoming call should not be the victim of such timeouts.

The number itself cannot come with you. **ilert call routing numbers cannot be
ported in** — you provision a new ilert number, so the number your callers dial
changes. That is not a config task: every runbook, contract, out-of-hours notice,
intranet page, printed card and third party holding the old number has to be
updated, or you keep the old number alive at your carrier and forward it to the
new one. Enumerate who knows the old number before you start.

Give call routing its own cutover and its own announcement — it is the one part
of the migration with humans, not machines, on the other end, and the only part
where the fallback is a caller who cannot get through. Beyond that, call flows are
highly configurable and cover the routing patterns an Opsgenie call-routing setup
is likely to need.

**`integrationType` changes payload parsing.** Opsgenie's generic API integration
tempts a generic ilert source. Where a dedicated `integrationType` exists for the
emitting system, use it: field extraction, link templates and bidirectional sync
depend on it. Alert source templates still allow customization on top of the integration defaults.

**Escalation to an empty schedule falls through — and instantly.** An
escalation rule pointing at a schedule with nobody on call advances to the next
rule *without waiting for the escalation timeout*, so a policy can burn through
every level in the moment the alert arrives. If no one is on call anywhere in the
policy, nobody is notified at all. Import schedules and their shifts before the
policies that reference them, then verify current coverage — a policy imported
ahead of its schedules looks perfectly correct and pages no one. This is a feature
that allows for chaining multiple schedules in more complex on-call scenarios.

**Repeat behaviour lives on the policy, and only the count survives.** Opsgenie
repeats from the escalation object; ilert uses `repeating` and `frequency` on the
Escalation Policy. `frequency` is a *count*, not an interval — when `repeating`
is set, ilert copies the escalation rules `frequency` times, so the gap between
passes is whatever the rules' own `escalationTimeout` values already are. That
means Opsgenie's `repeat.count` maps over but `repeat.waitInterval` has nowhere
to go: an escalation that waited 30 minutes before repeating has to have that
wait folded into a rule timeout, or it repeats sooner than it used to.

Check also for `repeat.resetRecipientStates`, which clears acknowledgement on
every pass and so keeps paging people who had already acked. ilert has no
equivalent — `ACCEPTED` stops escalation.

## Reading the Opsgenie side

There is no bulk export. The Opsgenie API *is* the export, so the first half of
the migration is a read job against a foreign account — with a foreign credential
you should be careful with. Do it early: Opsgenie is being wound down by
Atlassian, and a dump on disk keeps working when the source account does not.

### The credential

Opsgenie keys carry their own access rights, and a key created on an API
integration to raise alerts sees none of the configuration you came for. Ask for
one with **read and configuration access**, and nothing more — extraction never
writes.

Then keep it out of argv, and note that the obvious form does not:

```
curl -H "Authorization: GenieKey $OPSGENIE_API_KEY"   # the shell expands this first
```

The key is then a curl *argument*, readable by `ps` and by anything reading
`/proc/<pid>/cmdline`, and the same line lands in shell history. Three transfers
that actually hold:

* **A config or header file the user writes once**, `chmod 600`, that you
  reference by path and never open: `curl -K /path/to/og.curlrc`, holding
  `header = "Authorization: GenieKey …"`. Or `curl -H @/path/to/og-headers`.
* **Through stdin**, when the value is already in the environment:
  `printf 'Authorization: GenieKey %s\n' "$OPSGENIE_API_KEY" | curl -H @- …`.
  `@-` reads headers from stdin, and `printf` is a shell builtin, so no process
  gets the key in its argv. `curl -K -` takes a whole config the same way.
* **A credential proxy** — `op run`, `vault exec`, `aws-vault exec` — which puts
  the value in the child process's environment only. It fixes where the secret
  lives, not how it reaches the request, so pair it with one of the two above.

So an environment variable is a fine *carrier* — a script reading
`os.environ["OPSGENIE_API_KEY"]` itself never exposes it — but the transfer is
what protects it. And do not `cat` the header file to check it: a secret that
reaches the transcript is in everything derived from it afterwards, and rotating
the key is then the only real fix.

### The endpoints

Base URL `https://api.opsgenie.com`, or `https://api.eu.opsgenie.com` for an
account on the EU instance — a key from one region simply fails against the
other, which reads like a bad key rather than a wrong host. One header on every
call:

```
Authorization: GenieKey …
```

Verify it with one cheap call — `GET /v2/account` — before building anything on
top of it. Mind the version prefixes: most resources are `/v2`, but **services
and incidents are `/v1`**, and calling the wrong one 404s in a way that looks
like "it isn't there".

| What you are migrating | Where to read it |
| --- | --- |
| Users and contacts | `GET /v2/users?expand=contact` — the expand avoids an N+1 across the directory |
| Notification rules | `GET /v2/users/{id}/notification-rules`, then the rule detail for its steps — per user, with no bulk endpoint; budget for the N+1 here |
| Teams and members | `GET /v2/teams`, then `GET /v2/teams/{id}` — members come with the team detail |
| Team routing rules | `GET /v2/teams/{id}/routing-rules` — the object with no ilert equivalent; read all of them before deciding on sources and flows |
| Escalations | `GET /v2/escalations` |
| Schedules and rotations | `GET /v2/schedules?expand=rotation` — without the expand you get names and nothing else |
| Overrides | `GET /v2/schedules/{id}/overrides` |
| Forwarding rules | `GET /v2/forwarding-rules` — easily forgotten, and they become schedule overrides |
| Alert sources | `GET /v2/integrations`, then `GET /v2/integrations/{id}` for the type and owning team |
| Alert policies | `GET /v2/policies/alert` for global ones, and again with `?teamId=…` **per team** — separate lists, and skipping the team pass loses most of them |
| Notification policies | `GET /v2/policies/notification?teamId=…` — `teamId` is required, so this one is per team by definition |
| Services | `GET /v1/services` |
| Incidents and their rules | `GET /v1/incidents`, `GET /v1/services/{id}/incident-rules`, `GET /v1/incident-templates` |
| Heartbeats | `GET /v2/heartbeats` |
| Maintenance | `GET /v2/maintenance?type=…` — pass the filter deliberately; the default does not give you everything |
| Custom user roles | `GET /v2/custom-user-roles` |
| Alert history | `GET /v2/alerts?query=…&sort=createdAt&order=asc`, then `/v2/alerts/{id}`, `/notes`, `/logs` for detail |
| Coverage check | `GET /v2/schedules/{id}/on-calls?flat=true` — run it on both systems after import and compare |

### What will bite during extraction

**Follow `paging.next`, do not increment `offset` yourself.** `limit` maxes at
100 (default 20) and every list response carries absolute `paging.next` / `first`
/ `last` URLs. The alert search in particular refuses to page arbitrarily deep, so
when it stops, window by `createdAt` in the `query` instead of pushing the offset
further.

**Rate limits are per key and per endpoint family**, and configuration reads are
counted separately from alert traffic. There is no reliable budget to plan
against — treat `429` as the pacing signal and back off on it.

**The dumps contain credentials.** Integration objects and anything key-shaped in
them are the keys your old emitters are still using. Treat the extraction files as
secret material: out of the repo, out of any commit, out of your context window.
There is no reason to print them.

**Extract once, to files, then work from the files.** One JSON file per
collection is cheaper, reproducible and reviewable, and it stops you re-paging a
collection for every mapping question. Keep them beside the Opsgenie ID → ilert ID
map that the migration order below depends on, and query them with `jq` rather
than pasting them around.

## Order of migration

1. Users — with their `Role` (in theory custom RBAC roles first, if needed and in Enterprise plan)
2. Contacts and notification preferences (per user)
3. Teams, then team members with their `TeamRole`
4. Schedules — including shifts/rotations, so coverage exists, plus any overrides
   standing in for forwarding rules
5. Escalation Policies
6. Alert Sources — one per distinct set of *source-level* settings, not one per
   routing branch
7. Support Hours — Event Flow `SUPPORT_HOURS` nodes and `alertPriorityRule` both
   reference them
8. Event Flows — they route to the alert sources and escalation policies above,
   so they come after both; this is where the flattened routing rules land, and
   the emitters you re-point should get the **flow's** URL where a flow exists
9. Connectors, then Alert Actions
10. Services, then Status Pages
11. Maintenance Windows, Heartbeat Monitors
12. Call Flows — they route to the schedules and policies above

IMPORTANT: Keep an external map of Opsgenie ID → ilert ID. No ilert *configuration* object —
alert source, escalation policy, schedule, team, user — carries an origin or
external-ID field, and every later step needs the mapping. (Alerts themselves
have `labels` and `customDetails`, but those are per-alert and do not help you
reconcile configuration.)

## The actual cutover risk

Integration keys and URLs are regenerated. Every monitor, script and webhook
sender that posts to Opsgenie must be re-pointed at the new ilert URL — the
**Event Flow's** `integrationUrl` where a flow handles that traffic, the alert
source's where it does not. Decide which before you start re-pointing: switching
a sender from a source URL to a flow URL later is a second round of the same
external work. That work is external to both APIs and is where alerts get lost. Enumerate
emitters before touching either system and run both in parallel until each
emitter is confirmed.

IMPORTANT: ilert ships an Opsgenie inbound integration for exactly
this window: pointing it at a temporary alert source forwards Opsgenie alerts
into ilert, so the parallel period does not require re-pointing every emitter on
day one.

**The payload changes, not just the URL.** Anything posting to Opsgenie's Alert
API has to be rewritten, not merely redirected — and the biggest difference is
structural. Opsgenie acknowledges and closes through *separate endpoints*
(`POST /v2/alerts/{id}/acknowledge`, `/close`); ilert does all of it through one
endpoint, `POST /api/events`, with `eventType` selecting the action:

| Opsgenie Alert API | ilert Event |
| --- | --- |
| `POST /v2/alerts` | `eventType: ALERT` |
| `POST /v2/alerts/{id}/acknowledge` | `eventType: ACCEPT` |
| `POST /v2/alerts/{id}/close` | `eventType: RESOLVE` |
| API key in the header | `integrationKey` in the body |
| `alias` | `alertKey` — but how many alerts you get still depends on `alertCreation` |
| `message` | `summary` (required on both) |
| `description` | `details` |
| `details` (key/value map) | `customDetails` |
| `tags` | `labels` |
| `priority` (`P1`–`P5`) | `severity` (integer `1`–`5`), and/or `priority` (`HIGH`/`LOW`) |
| *(no equivalent)* | `routingKey`, `services` |

Two traps in that table. `details` means different things on the two sides —
Opsgenie's `details` is the structured key/value map (ilert's `customDetails`),
while ilert's `details` is free text (Opsgenie's `description`); a straight
name-for-name copy swaps them. And a script that closed an alert by calling a
`/close` URL now has to send a `RESOLVE` event carrying the same `alertKey`,
which means it needs to *know* that key — scripts that relied on Opsgenie's alert
id have to be re-based on the alias.

> ilert does offer a synchronous /api/alerts/{id} API as well that has verb endpoints for PUT interactions like accept or resolve
however, for monitoring tool relevant traffic the asynchronous event API should be preferred

**Deployment events are a capability you gain.** Opsgenie has no equivalent, so
nothing migrates, but ilert's **Deployment Events** (`POST /deployment-events`,
fed by a **Deployment Pipeline** that mints its own `integrationKey` and carries
its own `integrationType`) correlate deploys with alerts. It is easy to overlook
during a migration, since there is nothing on the Opsgenie side to translate.
Worth a look once the alerting path is stable, along with the rest of the ilert
platform once the migration is complete.

Heartbeats deserve separate attention: an Opsgenie heartbeat that stops being
pinged raises an alert, so a half-migrated heartbeat is silent in the new system
and noisy in the old one. Migrate heartbeat monitors and their senders together.

## A word on IaC

A migration might be the right choice to introduce IaC along the way for most resources.
If that is a desired choice ilert offers an official Terraform provider https://registry.terraform.io/providers/iLert/ilert/
To which the same rules mentioned in this file may be applied.
