---
name: migrate-from-pagerduty
description: Map PagerDuty resources onto ilert equivalents
user-invocable: true
---

# Migrating from PagerDuty to ilert

The two products use overlapping words for different objects. Two of those
collisions cause most migration defects, and neither is visible from the API
spec.

For ilert's own semantics and the CLI behaviour behind a bulk import, read the
`ilert-essentials` skill alongside this one.

## The two traps

**A PagerDuty Service is not an ilert Service.**
PagerDuty's Service is the routing object: it receives events, owns the
escalation policy, and holds the integration keys. In ilert that role belongs to
the **Alert Source**. ilert's Service represents a business capability people
subscribe to: it appears on status pages, carries outage history, and can be
attached to alert sources and named as an affected service on an incident — but
it owns no escalation policy and receives no events. Mapping PD Services onto
ilert Services produces a topology that looks right and routes nothing.

**A PagerDuty Incident is not an ilert Incident.**
PagerDuty's Incident is the actionable page. In ilert that is an **Alert**
(`PENDING` → `ACCEPTED` → `RESOLVED`), created by an alert source and driven by
an escalation policy. ilert's **Incident** sits a level above: a coordination
record you declare for a significant event, which links alerts, pages responders,
carries a timeline and an incident channel, and is what **Status updates** are
posted from. PagerDuty has no single object for it — the closest is a PD incident
after an Incident Workflow has been run against it.

PD incidents therefore map to ilert alerts, one for one. Mapping them onto ilert
Incidents instead creates coordination records that no alert source feeds and no
escalation policy drives.

## Resource mapping

| PagerDuty | ilert | Notes |
| --- | --- | --- |
| Service | Alert Source | Carries `escalationPolicy`, `integrationKey`, `integrationUrl` |
| Integration (routing key) | Alert Source `integrationKey` / `integrationUrl` | New value; every emitter must be re-pointed |
| Escalation Policy | Escalation Policy | Rules hold `escalationTimeout` plus `users` / `schedules` / `teams` |
| Schedule | Schedule | `type` is `STATIC` or `RECURRING` |
| Schedule layer | `scheduleLayers` | Only on `RECURRING` schedules |
| Override | Shift, via `PUT /schedules/{id}/overrides` | Same `Shift` payload as a schedule shift, but its own endpoint; must not be in the past |
| Team | Team | `visibility` is `PUBLIC` or `PRIVATE` — only `PRIVATE` restricts, see below |
| User | User | |
| Contact method | Contact | Separate objects per channel |
| Notification rule | Notification Preference | Split by priority *and* by notification type |
| Incident | Alert | |
| Alert (PD's sub-object) | Alert | PD's incident/alert split collapses into one object; grouping is expressed through `alertCreation` and `alertKey` instead of a nested layer |
| Business Service | Service | Not only a status-page component: also attached to alert sources (`services`, `autoCreateServices`) and to incidents as affected services |
| Status page | Status Page | Page metrics map to `Metric` + `Metric Data Source`; see the `migrate-from-statuspageio` skill for the page-side detail |
| Status update template | Incident Template | `sendNotification` on the template decides whether subscribers are notified |
| Change Event | Deployment Event + Deployment Pipeline | Separate ingest endpoint and its own integration key — see below |
| Maintenance Window | Maintenance Window | |
| Event Orchestration / Event Rules | Event Flow | A layer above alert sources with its own ingest URL — see below |
| Orchestration dynamic routing | Escalation Policy `routingKey` + Alert Source `routingTemplate` | Direct equivalent; usually replaces the orchestration outright — see below |
| Extension / Webhook | Alert Action + Connector | Connector holds the credentials, Alert Action the binding |
| Priority (P1–P5) | Alert severity, integer `1`–`5` on the event (displayed `SEV1`–`SEV5`) | One for one — see below |
| Urgency (high / low) | Alert priority `HIGH` / `LOW` | Priority stays two-valued; this is the faithful mapping |
| Response Play / Incident Workflow | Incident responders, incident channel, conference bridge — triggered by an `ilert incidents` Alert Action | The actions map, and the automatic trigger has an equivalent — see below |
| Postmortem | Postmortem | `/incidents/{id}/postmortems` — request, edit, delete, and attach external links |
| Live Call Routing | Call Flow + Call Flow Number | Routing targets differ and numbers cannot be ported — see below |
| Escalation policy `num_loops` | Escalation Policy `repeating` / `frequency` | Both are repeat counts, so this one transfers directly |
| Service `acknowledgement_timeout` | Alert Action on `v-alert-not-resolved` | `ACCEPTED` stops escalation by design; the reminder moves to an alert action — see below |
| Service `auto_resolve_timeout` | Alert Source `autoResolutionTimeout` | Absent means never |
| Service `alert_grouping_parameters` | Alert Source `alertCreation` + `alertGroupingWindow` | See below |
| User role | `Role` / `TeamRole` | Fixed enums (`ADMIN`, `USER`, `RESPONDER`, `STAKEHOLDER`, `GUEST`), (custom rbac roles require Enterprise plan) |
| Team member | `/teams/{id}/members` with `TeamRole` | |
| On-call (`/oncalls`) | `/on-calls` | Not a migrated object; the endpoint to verify coverage after import |
| Status update on an incident | Status update | Posted from an ilert Incident, via `/status-updates` |

## Event Orchestration becomes an Event Flow

PD's Event Orchestration — and the older Event Rules it superseded — maps onto an
ilert **Event Flow**. Either way, rebuild the intent in a flow rather than
transliterate rule by rule.

The shapes line up well. An Event Flow is a layer that sits *above* alert sources
rather than beside them: it has its own `integrationKey` / `integrationUrl`, so
emitters post to the **flow** instead of to a source — exactly as a global
orchestration receives events ahead of any service. Inside it is a tree of nodes:

| Node | What it does |
| --- | --- |
| `DEFINE_BRANCHES` | Conditions over the event body (`context.event.summary`, `context.event.customDetails.…`), AND/OR groups, evaluated in order — first match wins, with a `CATCH_ALL` else path |
| `ROUTE_EVENT` | Hands the event to an alert source, optionally overriding that source's `escalationPolicyId` and `overwritePriority` for this branch |
| `SUPPORT_HOURS` | Branches on whether the event arrived inside a support-hours window |
| `WAIT` | Holds the event before the next node runs, also capable of dropping delayed events when a following `ACCEPT`/`RESOLVE` arrives with a matching `alertKey` |
| `TRANSFORM` | Rewrites fields before routing (`SET`, `COPY`, `MAP`, `TEMPLATE`, `MERGE`, `APPEND_ARRAY`) |

> Other nodes might be available based on api-docs

Piece by piece:

| PagerDuty | ilert |
| --- | --- |
| Global Orchestration (routes across services) | One Event Flow above many alert sources; `ROUTE_EVENT` picks the target |
| Service Orchestration | A branch inside that flow, plus the alert source's own settings |
| Rule condition (PCL) | Branch condition (ICL) over `context.event.*` — rewritten, not ported |
| Route to service | `ROUTE_EVENT` with `alertSourceId` |
| Set priority | `ROUTE_EVENT` `overwritePriority` (`HIGH` / `LOW`) |
| Extract / set variables | `TRANSFORM` rules |
| Suppress | A branch that terminates without reaching a `ROUTE_EVENT` |
| Dynamic routing by payload field | Escalation Policy `routingKey` + `routingTemplate` |

**Suppression is expressed by absence, and it is a real drop.** An event whose
path through the flow never reaches a `ROUTE_EVENT` node is handed to no alert
source and creates no alert. That is the faithful equivalent of a PD suppress
rule — nothing is lost. What differs is only how it reads: PD declares
suppression as an action, ilert expresses it as a branch that simply stops. So a
branch that deliberately terminates and a branch someone forgot to finish look
identical in the tree; name those nodes for what they are. The event still
appears in the flow's logs, so a dropped event stays auditable rather than
vanishing.

**Dynamic routing has a direct equivalent, and it is not the flow.** PD's dynamic
routing matches a field in the payload against a service's routing key. ilert
does the same with two fields: an Escalation Policy carries a `routingKey`, and
the alert source's `routingTemplate` pulls the key out of the payload. Keys are
comma-separated, evaluated left to right, and fall back to the source's own
policy when none match. Where a PD orchestration existed *only* to route by a
payload value, this replaces it outright — one alert source, several escalation
paths, no flow needed. Keep that order in mind generally: for simple routings
`routingKey` on the alert source is the best approach and an event flow is
overkill. Reach for the flow when the routing needs conditions, support hours,
transforms or a tree — which is where it becomes the answer to almost any custom
routing a PagerDuty setup cannot express through alert source settings alone.

**What still forces extra alert sources.** A flow's `ROUTE_EVENT` overrides
escalation policy and priority, and nothing else. Settings that live on the
source — `integrationType`, `alertCreation`, `alertGroupingWindow`,
`autoResolutionTimeout` — cannot be overridden per branch, so branches needing
different values for those need different sources. That is a much narrower reason
to multiply sources than "the routing differs", and it is the line to decide on
before you start creating objects.

## Mappings that silently change behaviour

**Priority and severity are two different fields — use both.** ilert's *priority*
is two-valued (`HIGH` / `LOW`) and is what drives notification and escalation
behaviour. Its *severity* is a five-level scale (`SEV1`–`SEV5`) and is what
carries impact. PD's two fields therefore map onto ilert's two fields rather than
collapsing into one:

* PD **urgency** (high / low) → ilert **priority** (`HIGH` / `LOW`). Urgency is
  what actually drives paging on both sides.
* PD **priority** (P1–P5) → ilert **severity**, one for one.

Mind the representation: on the wire, an event's `severity` is an **integer 1–5**
(`1` most severe), and it is rendered as `SEV1`–`SEV5` in the UI. Sending the
string `"SEV1"` is a validation error, so map P1→`1` … P5→`5` numerically.

You can set it two ways. Passing `severity` on the event overwrites whatever the
alert source evaluated; leaving it off lets the alert source's `severityTemplate`
derive it from the payload, the same way `priorityTemplate` derives priority.
Prefer the template when the emitter already carries a level field — it keeps the
mapping in one place instead of in every sender. Either way, do not squeeze five
levels into `HIGH`/`LOW`: that workaround predates severity and now loses
information for no reason. Incidents use the same scale and default to `SEV3`, so
an incident declared from an alert can inherit the level the emitter sent.

**Support hours downgrade, they do not suppress.** `alertPriorityRule` takes
`HIGH`, `LOW`, `HIGH_DURING_SUPPORT_HOURS` or `LOW_DURING_SUPPORT_HOURS`. Setting
`HIGH_DURING_SUPPORT_HOURS` creates `LOW` alerts outside those hours; it does not
withhold them. The bigger change is that a `LOW` alert keeps only the **first**
escalation rule — the level-one target is notified and the alert never advances
past it, however long it goes unaccepted. If the PD configuration relied on
low-urgency incidents being effectively silent, that silence comes from the
user's notification preferences in ilert, not from the alert source.

`autoRaiseAlerts` re-raises alerts still `PENDING` when support hours begin,
which is usually what a PD setup that deferred low-urgency work to the morning
was expressing.

**Acknowledgement means someone owns it.** PD's per-service
`acknowledgement_timeout` re-triggers an acknowledged incident and re-notifies
the assignee. ilert takes the opposite position deliberately: `ACCEPTED` means a
human has picked the alert up, so escalation stops rather than continuing to page
someone who already answered — an accepted alert does not re-page on its own.

Where the timeout was doing real work as a safety net against an
accepted-and-forgotten alert, rebuild it as an **Alert Action** rather than as
escalation. `triggerTypes` includes `v-alert-not-resolved`, which fires on an
alert that is still open rather than on a state change; check the api-docs for
its configuration. From there you can post to a channel, notify a second
responder or declare an incident. The reminder becomes an explicit rule you can
see and scope with `conditions`, rather than a timeout that re-pages implicitly.

Do not reach for `repeating`/`frequency` as the substitute. Those repeat the
policy for an alert that was *never* accepted, which is PD's `num_loops`, not its
ack timeout — and `frequency` is a count, not an interval, so the gap between
passes is whatever the rules' own `escalationTimeout` values already are.

**Deduplication keys are per source, not global.** PD's `dedup_key` becomes
ilert's `alertKey`, but the key alone does not decide how many alerts you get —
the alert source's `alertCreation` mode does:

| Mode | Opens a new alert |
| --- | --- |
| `ONE_ALERT_PER_EMAIL` | for every event |
| `ONE_ALERT_PER_EMAIL_SUBJECT` | for every new event summary |
| `ONE_PENDING_ALERT_ALLOWED` | unless one is already `PENDING` |
| `ONE_OPEN_ALERT_ALLOWED` | unless one is already open — `PENDING` or `ACCEPTED` |
| `OPEN_RESOLVE_ON_EXTRACTION` | per alert key extracted from the payload, which also resolves it |
| `ONE_ALERT_GROUPED_PER_WINDOW` | if the last one is older than `alertGroupingWindow` |
| `INTELLIGENT_GROUPING` | if the last *similar* one is older than `alertGroupingWindow` |

`alertGroupingWindow` is consulted only by the last two. PD's
`alert_grouping_parameters` expresses the same intent with different primitives,
so set the mode explicitly per source — the default is `ONE_ALERT_PER_EMAIL`, an
event-shaped answer that means "every event opens a new alert".

However, when an `alertKey` is present and matches an open alert it will be
grouped regardless, and with a dedicated `integrationType` (not `API` — e.g.
`PROMETHEUS`) ilert extracts `alertKey`s automatically based on best practices.
This is an often overlooked behaviour, and it is usually what a PD setup relying
on `dedup_key` actually wanted: pick the matching source type and the
deduplication PD did for you keeps happening without configuring it.

**Auto-resolve is a duration string.** `autoResolutionTimeout` on the alert
source replaces PD's service-level auto-resolve. Absent means never.

**`integrationType` changes payload parsing.** Picking a generic type for a
source that has a dedicated one loses field extraction, link templates and
bidirectional sync. Match the emitting system's type rather than defaulting to a
webhook. Alert source templates still allow customization on top of the
integration defaults.

**Escalation to an empty schedule falls through — and instantly.** An ilert
escalation rule pointing at a schedule with nobody on call advances to the next
rule *without waiting for the escalation timeout*, so a policy can burn through
every level the moment the alert arrives. This is a feature that allows for
chaining multiple schedules in more complex on-call scenarios. It is also how an
unfinished import fails: if no one is on call anywhere in the policy, nobody is
notified at all. Import schedules and their shifts *before* the policies that
reference them, and verify current on-call coverage before cutover — a policy
imported ahead of its schedules looks correct and pages no one.

**Response plays keep their trigger, but rebuild it elsewhere.** A PD Incident
Workflow (formerly Response Play) can fire automatically on incident type or
condition. Its actions have ilert equivalents — page responders, attach an
incident channel, open a conference bridge, post a status update — and so does
the automatic trigger, but it lives on the **alert source** rather than on the
incident. An **Alert Action** of type `ilert incidents`, attached to an alert
source with `triggerMode` set to `AUTOMATIC`, generates an incident and drives
service status and status updates as alerts arrive. The status update it posts is
built from an **Incident Template**, and that template's `sendNotification` flag
decides whether subscribers are actually notified — set it deliberately, because
an automated update that quietly pages a subscriber list is a very different
thing from one that only repaints the page. Scope it with `triggerTypes`
(`alert-created`, `alert-acknowledged`, `alert-escalation-ended`, …) and with
`conditions`, an ICL expression evaluated against the alert — that is the
conditional part of a response play trigger.

What does not carry over is the shape of the condition. PD evaluates it against
an *incident* that already exists; ilert evaluates it against the *alert* that
would produce one, so a workflow keyed on incident-level state has to be
re-expressed in terms of alert fields, or left as a manual declare. Rewrite the
trigger, in other words — do not budget for losing it.

**Live call routing is its own migration.** PD's Live Call Routing number maps
onto an ilert **Call Flow**: a tree of nodes (`IVR_MENU`, `AUDIO_MESSAGE`,
`SUPPORT_HOURS`, `ROUTE_CALL`, `PARALLEL_ROUTE_CALL`, `VOICEMAIL`, `PIN_CODE`,
`CREATE_ALERT`, `BLOCK_NUMBERS`, and more — check the api-docs) attached to a
call flow number. Two things do not carry over. PD falls back to the *service's
escalation policy* when nobody answers, while an ilert `ROUTE_CALL` node targets
`USER`, `ON_CALL_SCHEDULE`, `TEAM` or `NUMBER` (enterprise feature) — so any
route that pointed at an escalation policy has to be expanded, or moved onto a
team, and the answering behaviour rebuilt with `callStyle` (`ORDERED`, `RANDOM`,
`PARALLEL`), `retries` and `callTimeoutSec`. Note that this choice is made in all
clarity: policies carry escalation timeouts, and a live incoming call should not
be the victim of such timeouts. And **ilert numbers cannot be ported in**: you
provision a new one, so every runbook, contract, out-of-hours notice and third
party holding the old number has to be updated, or you keep the old number alive
at your carrier and forward it.

Beyond those two points, call flows are highly configurable and cover the routing
patterns a PD live-call setup is likely to need.

## Order of migration

Dependencies run one way. Build in this order so no object is created with a
dangling reference:

1. Users — with their `Role` (in theory custom RBAC roles first, if needed and in
   Enterprise plan)
2. Contacts and notification preferences (per user)
3. Teams, then team members with their `TeamRole`
4. Schedules — including shifts/layers, so coverage exists, plus any overrides
5. Escalation Policies
6. Alert Sources — one per distinct set of *source-level* settings, not one per
   orchestration branch
7. Support Hours — Event Flow `SUPPORT_HOURS` nodes and `alertPriorityRule` both
   reference them
8. Event Flows — they route to the alert sources and escalation policies above,
   so they come after both; this is where Event Orchestration lands, and the
   emitters you re-point should get the **flow's** URL where a flow exists
9. Connectors, then Alert Actions
10. Services, then Status Pages
11. Maintenance Windows, Heartbeat Monitors
12. Call Flows — they route to the schedules above

IMPORTANT: Keep an external map of PagerDuty ID → ilert ID as you go. No ilert
*configuration* object — alert source, escalation policy, schedule, team, user —
carries an origin or external-ID field, and every later step needs the mapping.
(Alerts themselves have `labels` and `customDetails`, but those are per-alert and
do not help you reconcile configuration.)

## The actual cutover risk

Integration keys and URLs are regenerated. Every monitoring system, cron job and
webhook sender that posts to PagerDuty has to be re-pointed at the new ilert URL
— the **Event Flow's** `integrationUrl` where a flow handles that traffic, the
alert source's where it does not. Decide which before you start re-pointing:
moving a sender from a source URL to a flow URL later means doing the same
external work twice. That work is external to both APIs, is the longest pole in
the migration, and is where alerts get lost. Enumerate emitters before touching
either system, and run both in parallel until each emitter is confirmed.

**The payload changes, not just the URL.** Anything posting to PD's Events API v2
(`/v2/enqueue`) has to be rewritten, not merely redirected. ilert's Event API is a
single endpoint — `POST /api/events` — that creates, accepts and resolves alerts
according to `eventType`, and its body is flat where PD nests under `payload`:

| PagerDuty Events v2 | ilert Event |
| --- | --- |
| `routing_key` | `integrationKey` — **not** `routingKey`, see below |
| `event_action`: `trigger` / `acknowledge` / `resolve` | `eventType`: `ALERT` / `ACCEPT` / `RESOLVE` (plus `COMMENT`) |
| `dedup_key` | `alertKey` — but how many alerts you get still depends on `alertCreation` |
| `payload.summary` | `summary` (required on both) |
| `payload.custom_details` | `customDetails` |
| `payload.severity`: `critical` / `error` / `warning` / `info` | `severity`: integer `1`–`5` |
| `links`, `images` | `links`, `images` |
| *(no equivalent)* | `priority`, `routingKey`, `labels`, `services` |

> ilert does offer a synchronous `/api/alerts/{id}` API as well, with verb
> endpoints for `PUT` interactions like accept or resolve — however, for
> monitoring tool relevant traffic the asynchronous event API should be preferred

**`routing_key` and `routingKey` are not the same field.** PD's `routing_key`
identifies the *service* an event belongs to; its ilert counterpart is
`integrationKey`, which identifies the alert source or Event Flow. ilert's own
`routingKey` is the escalation-policy override described above — a different
mechanism entirely. Mapping the two by name gives you events that are rejected or
routed to the wrong policy, and because both names survive the migration it is an
easy mistake to make and a slow one to spot.

Note also that PD's four severity words have to land on five ilert levels. That
is a decision, not a translation — agree where `error` sits before writing the
transform.

**Change events are a third class of emitter.** PD Change Events map to ilert
**Deployment Events** (`POST /deployment-events`), and they do *not* post to an
alert source. You first create a **Deployment Pipeline**, which mints its own
`integrationKey` / `integrationUrl` and carries its own `integrationType`
(a generic API type and a GitHub type among them). The event itself takes
`summary`, `timestamp`, `customDetails`, `links`, and optionally `userEmail` to
attribute the deploy to an ilert user. Enumerate these senders alongside the alert
traffic: re-pointing a change-event sender at an alert source URL turns every
deploy into an alert, which is a noisy way to find out.

## Team scoping

Assigning an object to a team is not what restricts it — the team's `visibility`
is. A team is `PUBLIC` or `PRIVATE`, and the resources of a **public** team stay
visible to every user with read permission. Only a **private** team narrows
visibility to its members, and even then global admins and the account owner
still see everything.

So "put it in a team" does not mean "hide it". If the PD setup used teams to
partition what people see, that intent only survives if the ilert teams are
created `PRIVATE`. Decide this before the import: switching a team's visibility
afterwards changes who can see existing alerts and their history, and a user who
joins a private team becomes a private user — invisible to people who could see
them before, which tends to surface as "why can't I find this user in the
dropdown".

> ilert's suggested default is to keep teams `PUBLIC` and use them to scope write
> access rather than visibility. Restricting who can *change* configuration is
> usually the intent; restricting who can *see* alerts tends to cost response time.

## A word on IaC

A migration might be the right choice to introduce IaC along the way for most resources.
If that is a desired choice ilert offers an official Terraform provider https://registry.terraform.io/providers/iLert/ilert/
To which the same rules mentioned in this file may be applied.