---
name: migrate-from-opsgenie
description: Map Opsgenie resources onto ilert equivalents, including the mappings that silently change semantics
user-invocable: true
---

# Migrating from Opsgenie to ilert

The hard part is not the object mapping — it is that the two products route
alerts through different objects. Opsgenie routes **team-first**; ilert routes
**source-first**. A simple setup maps one-to-one, but any configuration that
leaned on team routing rules routes differently after a faithful field-by-field
import — and parts of it will not route at all.

## The routing model difference

In Opsgenie an integration hands an alert to a **team**, and the team's routing
rules decide which escalation applies, with alert/notification policies filtering
along the way.

In ilert the **Alert Source** owns the escalation policy directly. There is no
object equivalent to a team routing rule, but three mechanisms cover the same
ground, and choosing between them decides how many objects you end up creating:

* **Routing keys.** An Escalation Policy carries a `routingKey`. An event that
  supplies a matching key overrides the alert source's own policy. Keys are
  comma-separated and evaluated left to right, falling back to the source's
  policy when none match, and the key can be pulled from the payload with the
  alert source's `routingTemplate`. This is the closest analogue to a routing
  rule: one alert source, several escalation paths.
* **Event Flows.** For branches expressible as a filter on event content.
* **Additional alert sources.** One per escalation path — needed when the branch
  depends on something the emitter cannot signal in its payload.

Teams in ilert carry no routing rules, but they are not purely decorative: an
escalation rule can target a team directly, so teams do appear in notification
paths.

Flatten the routing rules first, on paper, before creating anything. The count of
alert sources is a migration decision, not a translation.

## Resource mapping

| Opsgenie | ilert | Notes |
| --- | --- | --- |
| Integration | Alert Source | Carries `escalationPolicy`, `integrationKey`, `integrationUrl` |
| API key / integration key | Alert Source `integrationKey` | New value; every emitter must be re-pointed |
| Team | Team | Visibility and ownership; carries no routing rules, but can be an escalation rule target |
| Team routing rule | Escalation Policy `routingKey`, Event Flow, or additional Alert Source | No single equivalent — see above |
| Escalation | Escalation Policy | Rules hold `escalationTimeout` plus `users` / `schedules` / `teams` |
| Schedule | Schedule | `type` is `STATIC` or `RECURRING` |
| Rotation | `scheduleLayers` | On `RECURRING` schedules |
| Override | Shift, via `PUT /schedules/{id}/overrides` | Same `Shift` payload as a schedule shift, but its own endpoint; must not be in the past |
| Alert | Alert | `PENDING` → `ACCEPTED` → `RESOLVED` |
| Alert `alias` | Alert `alertKey` | Deduplication identity |
| Incident (Incident Management) | Incident | Internal coordination on both sides: responders who are paged, associated/linked alerts, notes and timeline |
| Incident `responders` | Incident responders | ilert pages via escalation policy, schedule, individual or whole team |
| Incident `statusPageEntry` | Status update | The public half of an Opsgenie incident is a separate object in ilert, with its own `/status-updates` endpoint — see below |
| Incident rule (auto-create from alert) | *(no rule engine)* | ilert incidents are declared by a caller — UI, API or MCP — from scratch or from an alert |
| Service | Service | Not only a status-page component: also attached to alert sources (`services`, `autoCreateServices`) and to incidents as affected services |
| Status page | Status Page | |
| User | User | |
| Contact method | Contact | Separate objects per channel |
| Notification rule | Notification Preference | Split by priority *and* by notification type |
| Alert policy (`modify`) | Event Flow | Priority overrides, field rewriting, responder assignment |
| Notification policy (auto-close, dedup, delay) | Alert Source settings + Event Flow | Split across two places |
| Maintenance | Maintenance Window | |
| Heartbeat | Heartbeat Monitor | |
| Incoming call routing | Call Flow + Call Flow Number | A whole product area, easily forgotten — see below |
| Forwarding rule | Schedule override, per schedule | Narrower than Opsgenie's, and not a single object — see below |
| Custom user role | `Role` / `TeamRole` | Fixed enums (`ADMIN`, `USER`, `RESPONDER`, `STAKEHOLDER`, `GUEST`), not composable rights — lossy |
| Team member + team role | `/teams/{id}/members` with `TeamRole` | |
| Who is on call | `/on-calls` | Not a migrated object; the endpoint to verify coverage after import |
| Action / outgoing webhook | Alert Action + Connector | Connector holds credentials, Alert Action the binding |
| Priority P1–P5 | Alert priority `HIGH` / `LOW` | Lossy — see below |
| Escalation rule `delay.timeAmount` | Escalation rule `escalationTimeout` | Per rule on both sides; minutes on both sides |
| Escalation `repeat` | Escalation Policy `repeating` / `frequency` | Moves from the escalation to the policy — see below |

## Mappings that silently change behaviour

**Priority collapses to two values.** ilert has `HIGH` and `LOW`. A common,
defensible split is P1/P2 → `HIGH` and P3–P5 → `LOW`, but confirm it against how
notification rules were actually written in Opsgenie, not against the label
names. Keep the original level in the payload and map it through the alert
source's `priorityTemplate` if you need it for reporting.

**Close and acknowledge are renamed, not redefined.** Opsgenie's *close* becomes
ilert's *resolve*; *acknowledge* becomes *accept*. Both products allow closing or
resolving an alert that was never acknowledged, and both keep the alert in
history afterwards — the lifecycle is the same shape, so do not budget for a
behavioural change here. What does break is anything reading the values:
automation counting "acknowledged then closed" transitions has to be re-pointed
at `ACCEPTED` and `RESOLVED`, and any report keyed on Opsgenie's five priorities
has two values left to work with.

**Alert policies and notification policies split across two objects.** Opsgenie
keeps de-duplication, auto-close and delay in *notification policies*, and field
rewriting in *alert policies*. In ilert, de-duplication and auto-close are alert
source settings (`alertCreation`, `alertGroupingWindow`, `autoResolutionTimeout`)
while rewriting is an Event Flow. Half of each Opsgenie policy therefore lands in
a different place. Reviewing only one of the two loses behaviour silently.

The two also differ in scope, which decides what you can even find: Opsgenie
notification policies exist only inside *team* policies, while alert policies
exist both globally and per team. A global alert policy belongs to no team and is
easy to miss entirely when migrating team by team.

**Deduplication is governed by mode, not just by key.** `alias` → `alertKey` is
the easy half. What decides how many alerts you actually get is the alert
source's `alertCreation` mode — it is the grouping rule, not a flag:

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
consulted only by the last two. Opsgenie expresses the same intent through a
`notification-deduplication` policy that is value- or frequency-based, so there
is no mode-for-mode correspondence to copy across. Decide per source what "the
same alert" should mean and set the mode explicitly — the inherited default is
`ONE_ALERT_PER_EMAIL_SUBJECT`, which is an email-shaped answer that rarely
matches what an API or monitoring source wants.

**Support hours downgrade, they do not suppress.** `alertPriorityRule` takes
`HIGH`, `LOW`, `HIGH_DURING_SUPPORT_HOURS` or `LOW_DURING_SUPPORT_HOURS`. Setting
`HIGH_DURING_SUPPORT_HOURS` makes alerts `LOW` outside those hours — they are
still created and still notify according to each user's `LOW` preferences. The
larger change is that a `LOW` alert keeps only the **first** escalation rule: the
level-one target is notified, and the alert never advances past it no matter how
long it goes unaccepted. Opsgenie configurations that used time
restrictions or a `notification-suppress` policy to mean "do not page" need the
quiet part expressed in notification preferences, not in support hours.

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
historical alerts and incidents do not migrate anyway — so this mapping matters
for understanding the two models and for anything that creates incidents going
forward, not for a bulk import. What it does mean concretely: Opsgenie's incident
rules automatically open an incident when alert data matches a condition, and
ilert has no rule engine that does this — every declare path runs on behalf of a
caller. Teams that leaned on incident rules are giving up automation, not
translating it.

**Forwarding rules have no global equivalent.** An Opsgenie forwarding rule
redirects *everything* aimed at one user to another for a date range — whether
that user was reached through a schedule, through an escalation rule, or by
direct assignment. ilert's nearest tools are both narrower: a schedule
**override** replaces a user on *one* schedule for a window, and a **coverage
request** asks a colleague to accept specific shifts (mobile app only, and it
must be accepted). Neither touches a user named directly as an escalation rule
target. Before assuming an override reproduces a forwarding rule, enumerate every
schedule and policy the forwarded user appears in — what looks like one object in
Opsgenie is usually several in ilert, and the ones you miss still page the absent
person.

**Call routing is its own migration.** Opsgenie's incoming call routing — a phone
number that routes callers to on-call staff, with auto-attendant menus, voicemail
and blocklists — maps onto ilert **Call Flows**: a tree of nodes (`IVR_MENU`,
`AUDIO_MESSAGE`, `SUPPORT_HOURS`, `ROUTE_CALL`, `PARALLEL_ROUTE_CALL`,
`VOICEMAIL`, `PIN_CODE`, `CREATE_ALERT`, `BLOCK_NUMBERS`) attached to a call flow
number. The node structure transfers, but **the routing targets do not line up**:
Opsgenie routes a call to a user, schedule, team or escalation policy, while an
ilert `ROUTE_CALL` node targets only `USER`, `ON_CALL_SCHEDULE` or `NUMBER`. Any
route that pointed at a team or an escalation policy has to be rebuilt — usually
as the schedule behind it, which silently drops the escalation fallback when
nobody answers. `callStyle` (`ORDERED`, `RANDOM`, `PARALLEL`), `retries` and
`callTimeoutSec` on the node are where you rebuild part of that behaviour.

The number itself cannot come with you. **ilert call routing numbers cannot be
ported in** — you provision a new ilert number, so the number your callers dial
changes. That is not a config task: every runbook, contract, out-of-hours notice,
intranet page, printed card and third party holding the old number has to be
updated, or you keep the old number alive at your carrier and forward it to the
new one. Enumerate who knows the old number before you start.

Give call routing its own cutover and its own announcement — it is the one part
of the migration with humans, not machines, on the other end, and the only part
where the fallback is a caller who cannot get through.

**`integrationType` changes payload parsing.** Opsgenie's generic API integration
tempts a generic ilert source. Where a dedicated `integrationType` exists for the
emitting system, use it: field extraction, link templates and bidirectional sync
depend on it.

**Escalation to an empty schedule falls through silently — and instantly.** An
escalation rule pointing at a schedule with nobody on call advances to the next
rule *without waiting for the escalation timeout*, so a policy can burn through
every level in the moment the alert arrives. If no one is on call anywhere in the
policy, nobody is notified at all. Import schedules and their shifts before the
policies that reference them, then verify current coverage — a policy imported
ahead of its schedules looks perfectly correct and pages no one.

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
equivalent — `ACCEPTED` stops escalation — so that behaviour has to be rebuilt
elsewhere or dropped as a deliberate decision rather than lost in translation.

## Order of migration

1. Users — with their `Role`; collapse Opsgenie custom roles onto the fixed enum
   first, since there is nowhere to put the leftover rights
2. Contacts and notification preferences (per user)
3. Teams, then team members with their `TeamRole`
4. Schedules — including shifts/rotations, so coverage exists, plus any overrides
   standing in for forwarding rules
5. Escalation Policies
6. Alert Sources (one per flattened routing path)
7. Connectors, then Alert Actions
8. Services, then Status Pages
9. Maintenance Windows, Event Flows, Heartbeat Monitors
10. Call Flows — they route to the schedules and policies above

Keep an external map of Opsgenie ID → ilert ID. No ilert *configuration* object —
alert source, escalation policy, schedule, team, user — carries an origin or
external-ID field, and every later step needs the mapping. (Alerts themselves
have `labels` and `customDetails`, but those are per-alert and do not help you
reconcile configuration.)

## The actual cutover risk

Integration keys and URLs are regenerated. Every monitor, script and webhook
sender that posts to Opsgenie must be re-pointed at the new ilert alert source
URL. That work is external to both APIs and is where alerts get lost. Enumerate
emitters before touching either system and run both in parallel until each
emitter is confirmed. ilert ships an Opsgenie inbound integration for exactly
this window: pointing it at a temporary alert source forwards Opsgenie alerts
into ilert, so the parallel period does not require re-pointing every emitter on
day one.

Heartbeats deserve separate attention: an Opsgenie heartbeat that stops being
pinged raises an alert, so a half-migrated heartbeat is silent in the new system
and noisy in the old one. Migrate heartbeat monitors and their senders together.
