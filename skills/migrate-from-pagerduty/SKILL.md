---
name: migrate-from-pagerduty
description: Map PagerDuty resources onto ilert equivalents, including the mappings that silently change semantics
user-invocable: true
---

# Migrating from PagerDuty to ilert

The two products use overlapping words for different objects. Two of those
collisions cause most migration defects, and neither is visible from the API
spec.

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
| Alert (PD's sub-object) | — | ilert has no alert-under-incident layer |
| Business Service | Service | Not only a status-page component: also attached to alert sources (`services`, `autoCreateServices`) and to incidents as affected services |
| Status page | Status Page | |
| Maintenance Window | Maintenance Window | |
| Event Orchestration / Event Rules | Event Flow | |
| Extension / Webhook | Alert Action + Connector | Connector holds the credentials, Alert Action the binding |
| Priority (P1–P5) | Alert priority `HIGH` / `LOW` | Lossy — see below |
| Urgency (high / low) | Alert priority `HIGH` / `LOW` | This is the faithful mapping |
| Response Play / Incident Workflow | Incident responders, incident channel, conference bridge | The actions map; the automatic trigger does not — see below |
| Postmortem | Postmortem | `/incidents/{id}/postmortems` — request, edit, delete, and attach external links |
| Live Call Routing | Call Flow + Call Flow Number | Routing targets differ and numbers cannot be ported — see below |
| Escalation policy `num_loops` | Escalation Policy `repeating` / `frequency` | Both are repeat counts, so this one transfers directly |
| Service `acknowledgement_timeout` | *(no equivalent)* | `ACCEPTED` stops escalation — see below |
| Service `auto_resolve_timeout` | Alert Source `autoResolutionTimeout` | Absent means never |
| Service `alert_grouping_parameters` | Alert Source `alertCreation` + `alertGroupingWindow` | See below |
| User role | `Role` / `TeamRole` | Fixed enums (`ADMIN`, `USER`, `RESPONDER`, `STAKEHOLDER`, `GUEST`), not composable rights — lossy |
| Team member | `/teams/{id}/members` with `TeamRole` | |
| On-call (`/oncalls`) | `/on-calls` | Not a migrated object; the endpoint to verify coverage after import |
| Status update on an incident | Status update | Posted from an ilert Incident, via `/status-updates` |

## Mappings that silently change behaviour

**Priority is two-valued.** ilert has `HIGH` and `LOW`, nothing else. Map PD
*urgency*, not PD *priority*; urgency is what actually drives notification
behaviour on both sides. If P1–P5 matters for reporting, carry it in the alert
payload and surface it via the alert source's `priorityTemplate` — do not try to
encode five levels in a two-valued field.

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

**Acknowledgement re-escalation has no destination.** PD's per-service
`acknowledgement_timeout` re-triggers an acknowledged incident and re-notifies
the assignee. ilert has no equivalent: `ACCEPTED` stops escalation, full stop.
This is a capability being dropped, not relocated — decide deliberately what
replaces it, or accept that an acknowledged-and-forgotten alert stays quiet.

Do not reach for `repeating`/`frequency` as the substitute. Those repeat the
policy for an alert that was *never* accepted, which is PD's `num_loops`, not its
ack timeout — and `frequency` is a count, not an interval, so the gap between
passes is whatever the rules' own `escalationTimeout` values already are.

**Deduplication keys are per source, not global.** PD's `dedup_key` becomes
ilert's `alertKey`, but the key alone does not decide how many alerts you get —
the alert source's `alertCreation` mode does:

| Mode | Opens a new alert |
| --- | --- |
| `ONE_ALERT_PER_EMAIL` | for every email |
| `ONE_ALERT_PER_EMAIL_SUBJECT` | for every new email subject |
| `ONE_PENDING_ALERT_ALLOWED` | unless one is already `PENDING` |
| `ONE_OPEN_ALERT_ALLOWED` | unless one is already open — `PENDING` or `ACCEPTED` |
| `OPEN_RESOLVE_ON_EXTRACTION` | per alert key extracted from the payload, which also resolves it |
| `ONE_ALERT_GROUPED_PER_WINDOW` | if the last one is older than `alertGroupingWindow` |
| `INTELLIGENT_GROUPING` | if the last *similar* one is older than `alertGroupingWindow` |

The two email modes apply only to email sources, and `alertGroupingWindow` is
consulted only by the last two. PD's `alert_grouping_parameters` expresses the
same intent with different primitives, so set the mode explicitly per source —
the inherited default is `ONE_ALERT_PER_EMAIL_SUBJECT`, an email-shaped answer
that rarely matches what an API or monitoring source wants.

**Auto-resolve is a duration string.** `autoResolutionTimeout` on the alert
source replaces PD's service-level auto-resolve. Absent means never.

**`integrationType` changes payload parsing.** Picking a generic type for a
source that has a dedicated one loses field extraction, link templates and
bidirectional sync. Match the emitting system's type rather than defaulting to a
webhook.

**Escalation to an empty schedule falls through silently — and instantly.** An
ilert escalation rule pointing at a schedule with nobody on call advances to the
next rule *without waiting for the escalation timeout*, so a policy can burn
through every level the moment the alert arrives. If no one is on call anywhere
in the policy, nobody is notified at all. Import schedules and their shifts
*before* the policies that reference them, and verify current on-call coverage
before cutover — a policy imported ahead of its schedules looks correct and pages
no one.

**Response plays lose their trigger.** A PD Incident Workflow (formerly Response
Play) can fire automatically on incident type or condition. Its actions have ilert
equivalents — page responders, attach an incident channel, open a conference
bridge, post a status update — but they hang off an ilert **Incident**, and ilert
has no rule engine that declares one. Every declare path runs on behalf of a
caller. Automatic response plays become a manual step, which is a process change
to agree with the team, not a config detail.

**Live call routing is its own migration.** PD's Live Call Routing number maps
onto an ilert **Call Flow**: a tree of nodes (`IVR_MENU`, `AUDIO_MESSAGE`,
`SUPPORT_HOURS`, `ROUTE_CALL`, `PARALLEL_ROUTE_CALL`, `VOICEMAIL`, `PIN_CODE`,
`CREATE_ALERT`, `BLOCK_NUMBERS`) attached to a call flow number. Two things do
not carry over. PD falls back to the *service's escalation policy* when nobody
answers, while an ilert `ROUTE_CALL` node targets only `USER`,
`ON_CALL_SCHEDULE` or `NUMBER` — rebuild that fallback with `callStyle`
(`ORDERED`, `RANDOM`, `PARALLEL`), `retries` and `callTimeoutSec`, or it is
silently gone. And **ilert numbers cannot be ported in**: you provision a new one,
so every runbook, contract, out-of-hours notice and third party holding the old
number has to be updated, or you keep the old number alive at your carrier and
forward it.

## Order of migration

Dependencies run one way. Build in this order so no object is created with a
dangling reference:

1. Users — with their `Role`; collapse PD roles onto the fixed enum first, since
   there is nowhere to put the leftover rights
2. Contacts and notification preferences (per user)
3. Teams, then team members with their `TeamRole`
4. Schedules — including shifts/layers, so coverage exists, plus any overrides
5. Escalation Policies
6. Alert Sources
7. Connectors, then Alert Actions
8. Services, then Status Pages
9. Maintenance Windows, Event Flows, Heartbeat Monitors
10. Call Flows — they route to the schedules above

Keep an external map of PagerDuty ID → ilert ID as you go. No ilert
*configuration* object — alert source, escalation policy, schedule, team, user —
carries an origin or external-ID field, and every later step needs the mapping.
(Alerts themselves have `labels` and `customDetails`, but those are per-alert and
do not help you reconcile configuration.)

## The actual cutover risk

Integration keys and URLs are regenerated. Every monitoring system, cron job and
webhook sender that posts to PagerDuty has to be re-pointed at the new ilert
alert source URL. That work is external to both APIs, is the longest pole in the
migration, and is where alerts get lost. Enumerate emitters before touching
either system, and run both in parallel until each emitter is confirmed.

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
