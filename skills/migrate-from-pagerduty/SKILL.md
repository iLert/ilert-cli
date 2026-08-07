---
name: migrate-from-pagerduty
description: Map PagerDuty resources onto ilert equivalents, including the mappings that silently change semantics
---

# Migrating from PagerDuty to ilert

The two products use overlapping words for different objects. Two of those
collisions cause most migration defects, and neither is visible from the API
spec.

## The two traps

**A PagerDuty Service is not an ilert Service.**
PagerDuty's Service is the routing object: it receives events, owns the
escalation policy, and holds the integration keys. In ilert that role belongs to
the **Alert Source**. ilert's Service is a customer-facing component used for
status pages and outage history — it has no escalation policy and receives no
events. Mapping PD Services onto ilert Services produces a topology that looks
right and routes nothing.

**A PagerDuty Incident is not an ilert Incident.**
PagerDuty's Incident is the actionable page. In ilert that is an **Alert**
(`PENDING` → `ACCEPTED` → `RESOLVED`). ilert's Incident is the communication
object shown on a status page, referencing affected Services. A migration that
imports PD incident history into ilert Incidents publishes years of outages to
the status page.

## Resource mapping

| PagerDuty | ilert | Notes |
| --- | --- | --- |
| Service | Alert Source | Carries `escalationPolicy`, `integrationKey`, `integrationUrl` |
| Integration (routing key) | Alert Source `integrationKey` / `integrationUrl` | New value; every emitter must be re-pointed |
| Escalation Policy | Escalation Policy | Rules hold `escalationTimeout` plus `users` / `schedules` / `teams` |
| Schedule | Schedule | `type` is `STATIC` or `RECURRING` |
| Schedule layer | `scheduleLayers` | Only on `RECURRING` schedules |
| Override | Shift (`shifts`) | Overrides are not a separate object |
| Team | Team | |
| User | User | |
| Contact method | Contact | Separate objects per channel |
| Notification rule | Notification Preference | Split by priority *and* by notification type |
| Incident | Alert | |
| Alert (PD's sub-object) | — | ilert has no alert-under-incident layer |
| Business Service | Service | Status-page component |
| Status page | Status Page | |
| Maintenance Window | Maintenance Window | |
| Event Orchestration / Event Rules | Event Flow | |
| Extension / Webhook | Alert Action + Connector | Connector holds the credentials, Alert Action the binding |
| Priority (P1–P5) | Alert priority `HIGH` / `LOW` | Lossy — see below |
| Urgency (high / low) | Alert priority `HIGH` / `LOW` | This is the faithful mapping |
| Response Play | — | No equivalent |
| Postmortem | — | No equivalent |

## Mappings that silently change behaviour

**Priority is two-valued.** ilert has `HIGH` and `LOW`, nothing else. Map PD
*urgency*, not PD *priority*; urgency is what actually drives notification
behaviour on both sides. If P1–P5 matters for reporting, carry it in the alert
payload and surface it via the alert source's `priorityTemplate` — do not try to
encode five levels in a two-valued field.

**Support hours downgrade, they do not suppress.** An alert source with
`alertPriorityRule` set to `HIGH_DURING_SUPPORT_HOURS` creates `LOW` alerts
outside those hours. It does not withhold them. If the PD configuration relied on
low-urgency incidents being effectively silent, that silence comes from the
user's notification preferences in ilert, not from the alert source.

**Acknowledgement re-escalation moves.** PD re-escalates from a per-service
acknowledgement timeout. ilert has no per-source ack timeout; an `ACCEPTED`
alert stops escalating. Repeat behaviour lives on the Escalation Policy
(`repeating`, `frequency`), which is a different trigger with a different clock.
Configurations that depended on ack timeouts need rebuilding, not translating.

**Deduplication keys are per source, not global.** PD's `dedup_key` becomes
ilert's `alertKey`, but grouping is governed by the alert source's
`alertCreation` mode (`ONE_OPEN_ALERT_ALLOWED`, `ONE_ALERT_GROUPED_PER_WINDOW`,
…) together with `alertGroupingWindow`. The same key under a different
`alertCreation` mode produces a different number of alerts. Check this per
source; the default is not equivalent to PD's behaviour.

**Auto-resolve is a duration string.** `autoResolutionTimeout` on the alert
source replaces PD's service-level auto-resolve. Absent means never.

**`integrationType` changes payload parsing.** Picking a generic type for a
source that has a dedicated one loses field extraction, link templates and
bidirectional sync. Match the emitting system's type rather than defaulting to a
webhook.

**Escalation to an empty schedule falls through silently.** An ilert escalation
rule pointing at a schedule with nobody on call advances to the next rule without
warning. Import schedules and their shifts *before* the policies that reference
them, and verify current on-call coverage before cutover.

## Order of migration

Dependencies run one way. Build in this order so no object is created with a
dangling reference:

1. Users
2. Contacts and notification preferences (per user)
3. Teams
4. Schedules — including shifts/layers, so coverage exists
5. Escalation Policies
6. Alert Sources
7. Connectors, then Alert Actions
8. Services, then Status Pages
9. Maintenance Windows, Event Flows, Heartbeat Monitors

Keep an external map of PagerDuty ID → ilert ID as you go. Nothing in ilert
records the origin ID, and every later step needs it.

## The actual cutover risk

Integration keys and URLs are regenerated. Every monitoring system, cron job and
webhook sender that posts to PagerDuty has to be re-pointed at the new ilert
alert source URL. That work is external to both APIs, is the longest pole in the
migration, and is where alerts get lost. Enumerate emitters before touching
either system, and run both in parallel until each emitter is confirmed.

## Team scoping

Objects created without a team are visible account-wide. Objects assigned to
teams are restricted. PagerDuty's team model is advisory by default; ilert's is
enforced. Decide the scoping model before the import, because reassigning after
the fact changes who can see existing alerts.
