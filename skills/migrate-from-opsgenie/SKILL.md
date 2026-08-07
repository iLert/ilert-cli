---
name: migrate-from-opsgenie
description: Map Opsgenie resources onto ilert equivalents, including the mappings that silently change semantics
---

# Migrating from Opsgenie to ilert

The hard part is not the object mapping — it is that the two products route
alerts through different objects. Opsgenie routes **team-first**; ilert routes
**source-first**. A faithful field-by-field import produces a configuration that
never fires.

## The routing model difference

In Opsgenie an integration hands an alert to a **team**, and the team's routing
rules decide which escalation applies, with alert/notification policies filtering
along the way.

In ilert the **Alert Source** owns the escalation policy directly. There is no
team routing rule layer. So one Opsgenie integration whose team has three routing
rules usually becomes *three ilert alert sources*, one per resulting escalation
path — or one alert source plus Event Flows, if the branch is expressible as a
filter. Teams in ilert control visibility and ownership, not routing.

Flatten the routing rules first, on paper, before creating anything. The count of
alert sources is a migration decision, not a translation.

## Resource mapping

| Opsgenie | ilert | Notes |
| --- | --- | --- |
| Integration | Alert Source | Carries `escalationPolicy`, `integrationKey`, `integrationUrl` |
| API key / integration key | Alert Source `integrationKey` | New value; every emitter must be re-pointed |
| Team | Team | Visibility and ownership only |
| Team routing rule | Additional Alert Source, or Event Flow | No direct equivalent — see above |
| Escalation | Escalation Policy | Rules hold `escalationTimeout` plus `users` / `schedules` / `teams` |
| Schedule | Schedule | `type` is `STATIC` or `RECURRING` |
| Rotation | `scheduleLayers` | On `RECURRING` schedules |
| Override | Shift (`shifts`) | Not a separate object |
| Alert | Alert | `PENDING` → `ACCEPTED` → `RESOLVED` |
| Alert `alias` | Alert `alertKey` | Deduplication identity |
| Incident (Incident Management) | Incident | Status-page communication object |
| Service | Service | Status-page component, referenced by `affectedServices` |
| Status page | Status Page | |
| User | User | |
| Contact method | Contact | Separate objects per channel |
| Notification rule | Notification Preference | Split by priority *and* by notification type |
| Alert policy | Event Flow | Priority overrides, field rewriting |
| Notification policy (auto-close, dedup, delay) | Alert Source settings + Event Flow | Split across two places |
| Maintenance | Maintenance Window | |
| Heartbeat | Heartbeat Monitor | |
| Action / outgoing webhook | Alert Action + Connector | Connector holds credentials, Alert Action the binding |
| Priority P1–P5 | Alert priority `HIGH` / `LOW` | Lossy — see below |
| Escalation "notify next if not acked" | Escalation rule `escalationTimeout` | Per rule, not per escalation |

## Mappings that silently change behaviour

**Priority collapses to two values.** ilert has `HIGH` and `LOW`. A common,
defensible split is P1/P2 → `HIGH` and P3–P5 → `LOW`, but confirm it against how
notification rules were actually written in Opsgenie, not against the label
names. Keep the original level in the payload and map it through the alert
source's `priorityTemplate` if you need it for reporting.

**Close and acknowledge are not symmetric.** Opsgenie's *close* becomes ilert's
*resolve*; *acknowledge* becomes *accept*. Opsgenie can close an alert that was
never acknowledged and it simply disappears; in ilert, resolving from `PENDING`
is likewise allowed but the alert remains in history attached to its source. Any
automation that counted "acknowledged then closed" transitions will see a
different shape.

**Alert policies and notification policies split across two objects.** Opsgenie
keeps de-duplication, auto-close and delay in *notification policies*, and field
rewriting in *alert policies*. In ilert, de-duplication and auto-close are alert
source settings (`alertCreation`, `alertGroupingWindow`, `autoResolutionTimeout`)
while rewriting is an Event Flow. Half of each Opsgenie policy therefore lands in
a different place. Reviewing only one of the two loses behaviour silently.

**Deduplication is governed by mode, not just by key.** `alias` → `alertKey` is
the easy half. The number of alerts produced depends on the alert source's
`alertCreation` mode (`ONE_OPEN_ALERT_ALLOWED`,
`ONE_ALERT_GROUPED_PER_WINDOW`, …). The default mode is not equivalent to
Opsgenie's default de-duplication. Set it explicitly per source.

**Support hours downgrade, they do not suppress.** Setting `alertPriorityRule` to
`HIGH_DURING_SUPPORT_HOURS` makes alerts `LOW` outside those hours — they are
still created and still notify according to each user's `LOW` preferences.
Opsgenie configurations that used time restrictions to mean "do not page" need
the quiet part expressed in notification preferences.

**`integrationType` changes payload parsing.** Opsgenie's generic API integration
tempts a generic ilert source. Where a dedicated `integrationType` exists for the
emitting system, use it: field extraction, link templates and bidirectional sync
depend on it.

**Escalation to an empty schedule falls through silently.** An escalation rule
pointing at a schedule with nobody on call advances to the next rule without
warning. Import schedules and their shifts before the policies that reference
them, then verify current coverage.

**Repeat behaviour lives on the policy.** Opsgenie repeats an escalation from the
escalation object; ilert uses `repeating` and `frequency` on the Escalation
Policy. Check that a repeating escalation was not also relied upon to re-notify
after acknowledgement — in ilert, `ACCEPTED` stops escalation.

## Order of migration

1. Users
2. Contacts and notification preferences (per user)
3. Teams
4. Schedules — including shifts/rotations, so coverage exists
5. Escalation Policies
6. Alert Sources (one per flattened routing path)
7. Connectors, then Alert Actions
8. Services, then Status Pages
9. Maintenance Windows, Event Flows, Heartbeat Monitors

Keep an external map of Opsgenie ID → ilert ID. Nothing in ilert records the
origin ID, and every later step needs it.

## The actual cutover risk

Integration keys and URLs are regenerated. Every monitor, script and webhook
sender that posts to Opsgenie must be re-pointed at the new ilert alert source
URL. That work is external to both APIs and is where alerts get lost. Enumerate
emitters before touching either system and run both in parallel until each
emitter is confirmed.

Heartbeats deserve separate attention: an Opsgenie heartbeat that stops being
pinged raises an alert, so a half-migrated heartbeat is silent in the new system
and noisy in the old one. Migrate heartbeat monitors and their senders together.
