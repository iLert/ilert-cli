---
name: migrate-from-statuspage
description: Map Statuspage.io pages, components and incidents onto ilert status pages, including the mappings that silently change semantics
user-invocable: true
---

# Migrating from Statuspage.io to ilert

The object model lines up unusually well — component statuses map one for one, and
so do incident statuses. That makes this migration feel like a copy job, which is
the danger. What breaks is not the objects; it is **who gets told, and when**, and
a subscriber list that cannot simply be moved.

## The trap: changing a status does not tell anyone

In Statuspage, a component status change is itself a customer-facing event. It
updates the page, fires webhooks and can notify component subscribers, with no
incident involved.

In ilert a service's status and the communication about it are deliberately
separate. **Updating a service's status directly does not notify subscribers.**
Subscribers are notified only when a **Status update** is posted naming that
service among its affected services. So a migration that faithfully reproduces
"set component X to partial outage" produces a page that looks correct and a
mailing list that heard nothing.

Anything automated against the Statuspage API to flip component statuses has to
be rewritten to post a status update, not to set a status — otherwise it goes
quiet the day you cut over, and quietly, which is the worst way for a status
page to fail.

## How the communication objects nest

Statuspage's Incident is standalone: you open it on the page and post updates to
it. ilert splits the same work across two objects. An **Incident** is the internal
coordination record — linked alerts, responders, timeline — and a **Status update**
is the public message posted *from* it, which is what lands on the status page and
reaches subscribers.

For a pure status-page migration you mostly want status updates. Do not map
Statuspage incidents onto ilert Incidents: that creates internal coordination
records that publish nothing.

## Resource mapping

| Statuspage | ilert | Notes |
| --- | --- | --- |
| Page | Status Page | `subdomain`, `domain`, `visibility` (`PUBLIC`/`PRIVATE`), `appearance` (`LIGHT`/`DARK`), `pageLayout` (`SINGLE_COLUMN`/`RESPONSIVE`) |
| Component | Service | The subscribable unit on both sides |
| Component group | Status Page group | Expressed in `structure.elements` as a `GROUP` element with `children` |
| Component status | `ServiceStatus` | Exact 1:1 — see the status table below |
| Component showcase / ordering | `structure.elements` | Order and nesting live on the page, not on the service |
| Incident | Status update | Posted from an ilert Incident; the `/incidents` API resource is the Incident, `/status-updates` the update |
| Incident update | A further Status update | Same object, posted again with a new status |
| Incident status | `IncidentStatus` | `INVESTIGATING` / `IDENTIFIED` / `MONITORING` / `RESOLVED` — 1:1 |
| Incident impact (`none`/`minor`/`major`/`critical`) | *(no equivalent)* | Derived on Statuspage from component statuses; nothing in ilert carries it — see below |
| Scheduled maintenance | Maintenance Window with `services` | Dual-purpose in ilert — see below |
| Maintenance status (`scheduled`/`in_progress`/`verifying`/`completed`) | *(no equivalent)* | A window has `start` and `end`; the phase is implied |
| Incident template | Incident Template | |
| Subscriber (email / SMS / Slack / Teams / webhook) | Status page subscriber | Double opt-in — see below |
| Component subscription | Service subscription | Subscribers pick services, same idea |
| Private/internal subscriber | `/status-pages/{id}/private-subscribers` | Takes ilert users and teams, **not** arbitrary emails |
| Metrics | Metric | With a Metric Data Source behind it |
| Metric provider (Datadog, Pingdom, …) | Metric Data Source | `MetricDataSourceType` |
| IP restriction | `ipWhitelist` | |
| Page access users | `Role` / `TeamRole` | Fixed enums (`ADMIN`, `USER`, `RESPONDER`, `STAKEHOLDER`, `GUEST`), not composable rights |
| Status embed / badge | Floating widget / status badge | The floating widget shows only during an active incident or maintenance; the badge is always visible |
| Third-party components | *(no documented equivalent)* | Plan to model a component that mirrored an external vendor's page as a service you update yourself |

### Component status → service status

This is the cleanest part of the migration; take it literally.

| Statuspage | ilert |
| --- | --- |
| `operational` | `OPERATIONAL` |
| `degraded_performance` | `DEGRADED` |
| `partial_outage` | `PARTIAL_OUTAGE` |
| `major_outage` | `MAJOR_OUTAGE` |
| `under_maintenance` | `UNDER_MAINTENANCE` |

## Mappings that silently change behaviour

**Impact has nowhere to go.** Statuspage derives an incident's impact from the
statuses of its components — all major outages reads `critical`, any partial
outage reads `minor`, and so on — and that derived value drives the page banner
and, on many pages, whether people treat the notice as urgent. ilert has no impact
field on a status update. The severity signal a reader gets comes from the service
statuses themselves. If your incident history or reporting keys on impact, it does
not survive; decide whether to encode it in the update text rather than discover
it missing later.

**Uptime percentages will not match.** ilert computes service uptime with fixed
rules: `OPERATIONAL`, `DEGRADED` and `UNDER_MAINTENANCE` all count as uptime,
`MAJOR_OUTAGE` counts as downtime, and `PARTIAL_OUTAGE` counts at 30% of a major
outage. Statuspage computes its own showcase differently. Two pages showing the
same history will therefore quote different numbers, and ilert displays up to 90
days. If a published SLA figure is involved, work out the new number before the
page goes live rather than explaining a discrepancy afterwards.

**Maintenance windows do two jobs.** In Statuspage a scheduled maintenance is
purely communication. An ilert Maintenance Window takes both `services` *and*
`alertSources`: services move to `UNDER_MAINTENANCE` on every associated status
page and subscribers can be notified in advance, at the start and at the end,
while any attached alert sources are silenced for the duration. That is usually
an improvement — one object instead of a maintenance notice plus a separate
suppression — but only if you attach the alert sources deliberately. Attach none
and the page is honest while responders still get paged; attach the wrong ones and
you go blind to real failures during the window.

**Subscribers are the asset, and they do not move quietly.** This is the part to
plan first, not last. ilert status page subscriptions are confirmed by double
opt-in, and unconfirmed subscribers are chased with reminders after 24 hours,
three days and one week. An exported Statuspage subscriber list is therefore not
something you can silently switch on — the people on it have to consent again, and
some fraction never will. Export the list from Statuspage early, confirm with ilert
support what bulk import is actually possible on your plan, and treat attrition as
expected rather than as a failure. Note also that `private-subscribers` is a
different thing: it takes ilert users and teams, not customer email addresses.

**Audience-specific pages mean something narrower.** ilert's audience-specific
page is private and resolves its content from the viewer's team assignments, so
viewers must be authenticated ilert users or stakeholder accounts — and it
supports neither IP nor email whitelisting. A Statuspage audience-specific page
aimed at customers who simply hold a link does not translate; that is either a
`PRIVATE` page with an `ipWhitelist`, or a public page with less on it.

## Order of migration

1. Services — one per component, with its current status
2. Status Pages, then their `structure` (element order, groups, `expand` /
   `no-graph` options)
3. Metric Data Sources, then Metrics, then attach them to pages
4. Incident Templates
5. Maintenance Windows — services first, alert sources only where intended
6. Subscribers, last, once the page renders correctly

Historical incidents do not migrate. Neither do uptime histories: ilert starts
computing uptime from the statuses it observes, so the graph begins at cutover.
If the history matters, keep the Statuspage page readable for a period and link
to it rather than trying to backfill.

## The actual cutover risk

It is DNS, and it is one-way. A custom domain points at Statuspage by CNAME and
can point at only one provider at a time, so the moment you switch it every
existing bookmark, email footer, support macro and uptime checker follows — to a
page whose subscriber list is still filling up. Bring the ilert page fully live on
its `subdomain` first, verify it with real traffic, and switch the CNAME only when
the page is worth arriving at.

Run the two in parallel while that is true. Post to both during the overlap: a
status page that is stale during its own migration costs more trust than the
migration saves, and it is the one system whose failures your customers see
before you do.
