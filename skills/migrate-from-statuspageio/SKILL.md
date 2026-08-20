---
name: migrate-from-statuspageio
description: Map Statuspageio (Atlassian Statuspage) resources onto ilert status page equivalents
user-invocable: true
---

# Migrating from Statuspageio (Atlassian Statuspage) to ilert

The object model lines up unusually well — component statuses map one for one, and
so do incident statuses. That makes this migration feel like a copy job, which is
the danger. What breaks is not the objects; it is **who gets told, and when**.
The subscriber list does move — ilert has a dedicated import path that preserves
existing consent — but only if you use it deliberately.

For ilert's own semantics and the CLI behaviour behind a bulk import, read the
`ilert-essentials` skill alongside this one. Where Statuspage's own data lives —
and how to hold a foreign API key while you read it — is under *Reading the
Statuspage side* below; start there when the job is extraction rather than
design.

## Status and communication are two separate objects

In Statuspage, a component status change is itself a customer-facing event. It
updates the page, fires webhooks and can notify component subscribers, with no
incident involved.

ilert separates the two deliberately. A service's status describes what is
currently true; a **Status update** is what you choose to say about it.
**Updating a service's status directly does not notify subscribers** — they are
notified when a status update is posted naming that service among its affected
services. That split is what lets you correct a wrong status, move a service
around during a noisy investigation, or let automation drive status continuously,
without mailing your customer list every time the truth changes.

IMPORTANT: it does mean one thing has to change on the way in. Anything automated
against the Statuspage API to flip component statuses now has to post a status
update as well, not only set a status. A migration that transliterates "set
component X to partial outage" gives you a page that is accurate and an audience
that has not been told. Go through the automations one at a time and decide which
of the two each one wanted — usually both, and an **Incident Template** makes the
update side a single call.

> Another important fact is that in ilert services can be shared and reused in multiple status pages
whereas in Statuspageio each page gets its own component version of that same service

## How the communication objects nest

Statuspage's Incident is standalone: you open it on the page and post updates to
it. ilert splits the same work across two objects. An **Incident** is the internal
coordination record — linked alerts, responders, timeline — and a **Status update**
is the public message posted *from* it, which is what lands on the status page and
reaches subscribers.

For a pure status-page migration you mostly want status updates, so map a
Statuspage incident onto a **Status update** rather than onto an ilert Incident —
the Incident is the response-side object and publishes nothing on its own. Where
you want both, declare the Incident and post the update from it: that pairing is
the shape ilert is built around, and it gets you the coordination side that
Statuspage never had.

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
| Incident impact (`none`/`minor`/`major`/`critical`) | Read from the affected services' statuses | Kept in one place rather than as a second field beside the services — see below |
| Scheduled maintenance | Maintenance Window with `services` | Dual-purpose in ilert — see below |
| Maintenance status (`scheduled`/`in_progress`/`verifying`/`completed`) | Derived from the window's `start` and `end` | The phase follows the schedule instead of being advanced by hand |
| Incident template | Incident Template | |
| Subscriber (email / SMS / Slack / Teams / webhook) | Status page subscriber (`EMAIL` / `SMS` / `WEBHOOK`) | Double opt-in normally; import already-confirmed ones via `subscribers_bulk?import=true` — see below |
| Component subscription | `services` on the subscriber | Subscribers pick services, same idea; carried in the bulk import |
| Private/internal subscriber | `/status-pages/{id}/private-subscribers` | Takes ilert users and teams, **not** arbitrary emails |
| Metrics | Metric | With a Metric Data Source behind it |
| Metric provider (Datadog, Pingdom, …) | Metric Data Source | `MetricDataSourceType` |
| IP restriction | `ipWhitelist` | |
| Page access users | `Role` / `TeamRole` | Fixed enums (`ADMIN`, `USER`, `RESPONDER`, `STAKEHOLDER`, `GUEST`), (custom rbac roles require Enterprise plan) |
| Status embed / badge | Floating widget / status badge | The floating widget shows only during an active incident or maintenance; the badge is always visible |
| Third-party components | A Service you own | A component that mirrored an external vendor's page becomes a service you update — the same mirroring you were already doing, now under your control |

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

**Impact is read from the services, not declared alongside them.** Statuspage
derives an incident's impact from the statuses of its components — all major
outages reads `critical`, any partial outage reads `minor`, and so on — and that
derived value drives the page banner. ilert keeps the signal in one place: a
reader takes severity from the affected services' own statuses, so there is no
second field that can drift out of step with the services it claims to describe.
The practical consequence for the migration is that impact is not a value you
copy across. If your incident history or reporting keys on it, carry it in the
update text or recompute it from the affected services — worth settling before
the export, not after.

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
while any attached alert sources are silenced for the duration.

**Subscribers are the asset, and they migrate — if you use the import flag.**
A normally created ilert status page subscription is confirmed by double opt-in:
a confirmation notification goes out on creation, and unconfirmed subscribers are
chased with reminders after 24 hours, three days and one week. That is the wrong
path for a migration, because the people on an exported Statuspage list have
already opted in once.

ilert supports carrying that consent across. `POST` to
`/status-pages/{id}/subscribers_bulk?import=true` creates the subscribers as
**already confirmed**, with no confirmation notification sent. The body is an
array of objects with `target` (email address, phone number or URL), `type`
(`EMAIL`, `SMS` or `WEBHOOK`), `locale` (`en-GB` or `de-DE`) and an optional
`services` array of service IDs to scope the subscription — which is where a
Statuspage per-component subscription lands.

Three things to get right:

* **Do not omit `import=true`.** Without it the same call is an ordinary
  subscribe, and every person on your list gets a confirmation request — the one
  outcome the import exists to avoid.
* **Batch it.** Send 100–250 subscribers per call, and test with your own
  addresses and numbers before pushing thousands.
* **Deletion is not silent either.** Removing a confirmed subscriber afterwards
  sends an unsubscribe notification to the target, so clean up mistakes before
  the list is live, not after.

Note that `private-subscribers` is a different thing: it takes ilert users and
teams, not customer email addresses.

**Audience-specific pages key on identity, not on holding a link.** ilert's
audience-specific page is private and resolves its content from the viewer's team
assignments, so every viewer sees exactly the services they are entitled to and
nothing else — stronger than a shared secret URL, but it does require viewers to
be authenticated ilert users or stakeholder accounts. A Statuspage
audience-specific page aimed at customers who merely hold a link is a different
model, and it lands in one of two places: a `PRIVATE` page with an `ipWhitelist`,
or a public page scoped to the services you are content to show everyone. Choose
per audience rather than globally — the identity-based page is the better answer
wherever the viewers are known to you.

## Reading the Statuspage side

The Manage API is the export. It is also the API that hands you your customers'
email addresses, so this extraction needs more care than the others.

### The credential

A Statuspage key belongs to a **user** and inherits that user's access to the
pages in the organisation; there is no read-only variant, so read with it and
write nothing.

Keep it out of argv, and note that the obvious form does not:

```
curl -H "Authorization: OAuth $STATUSPAGE_API_KEY"   # the shell expands this first
```

The key is then a curl *argument*, readable by `ps` and by anything reading
`/proc/<pid>/cmdline`, and the same line lands in shell history. Three transfers
that actually hold:

* **A config or header file the user writes once**, `chmod 600`, that you
  reference by path and never open: `curl -K /path/to/sp.curlrc`, holding
  `header = "Authorization: OAuth …"`. Or `curl -H @/path/to/sp-headers`.
* **Through stdin**, when the value is already in the environment:
  `printf 'Authorization: OAuth %s\n' "$STATUSPAGE_API_KEY" | curl -H @- …`.
  `@-` reads headers from stdin, and `printf` is a shell builtin, so no process
  gets the key in its argv. `curl -K -` takes a whole config the same way.
* **A credential proxy** — `op run`, `vault exec`, `aws-vault exec` — which puts
  the value in the child process's environment only. It fixes where the secret
  lives, not how it reaches the request, so pair it with one of the two above.

So an environment variable is a fine *carrier* — a script reading
`os.environ["STATUSPAGE_API_KEY"]` itself never exposes it — but the transfer is
what protects it. And do not `cat` the header file to check it: a secret that
reaches the transcript is in everything derived from it afterwards. A leaked
Statuspage key can post to your public page, so rotate it rather than hope.

### The endpoints

Base URL `https://api.statuspage.io/v1`. One header on every call:

```
Authorization: OAuth …
```

Everything except `GET /pages` is scoped by a `page_id`, so start there — it is
also the cheap call that proves the key works.

| What you are migrating | Where to read it |
| --- | --- |
| The page itself | `GET /pages` — appearance, subdomain, custom domain, visibility |
| Components → services | `GET /pages/{page_id}/components?page=1&per_page=100` |
| Component groups | `GET /pages/{page_id}/component-groups` |
| Incidents and their updates | `GET /pages/{page_id}/incidents` (plus `/incidents/unresolved`) — each incident carries its `incident_updates` inline |
| Scheduled maintenance | `GET /pages/{page_id}/incidents/scheduled` and `/incidents/upcoming` — maintenance is an incident with `scheduled_for` / `scheduled_until` |
| Incident templates | `GET /pages/{page_id}/incident_templates` |
| Subscribers | `GET /pages/{page_id}/subscribers?type=email&state=active&page=1&per_page=100` — repeat per `type`; see below before you run it |
| Metrics | `GET /pages/{page_id}/metrics` and `GET /pages/{page_id}/metrics_providers` |
| Audience-specific access | `GET /pages/{page_id}/page_access_users`, `GET /pages/{page_id}/page_access_groups` |
| Embed / badge config | `GET /pages/{page_id}/status_embed_config` |

### What will bite during extraction

**The lazy call is the one that gets throttled.** The Manage API allows roughly
60 requests per minute on a rolling window *for paginated requests* — but an
**unpaginated** `GET` to `components` or `page_access_users` is limited to **one
request per minute**. Always send `?page=1&per_page=100`, even for a page with
three components; `page` is 1-based and `per_page` caps at 100. Rate-limited
responses are `429` and carry `Retry-After`: honour it rather than guessing.

**Subscriber data is personal data, and it is the one export you must not read.**
Write it straight to a file, count the rows, and hand the file to ilert's
`subscribers_bulk?import=true` path. Do not print it into the conversation, do not
summarise individual addresses, and do not leave it in the repo when you are done.
Export only the states you actually intend to import — importing an unconfirmed or
stale subscriber as confirmed is a consent decision, not a data-format decision.

**Extract once, to files, then work from the files.** One JSON file per collection
is cheaper, reproducible and reviewable, and with a 60-per-minute budget it is
also the difference between a five-minute extraction and an afternoon. Keep the
files beside the Statuspage ID → ilert ID map, and query them with `jq` rather
than re-fetching.

## Order of migration

1. Services — one per component, with its current status
2. Status Pages, then their `structure` (element order, groups, `expand` /
   `no-graph` options)
3. Metric Data Sources, then Metrics, then attach them to pages
4. Incident Templates
5. Maintenance Windows — services first, alert sources only where intended
6. Subscribers, last, once the page renders correctly — via
   `subscribers_bulk?import=true`, in batches, after a test batch of your own
   targets

Historical incidents and uptime histories stay behind, and often do not need to
come: ilert computes uptime from the statuses it observes, so the graph starts
clean at cutover. Where the history genuinely matters, keep the Statuspage page
readable for a period and link to it — cheaper and more honest than a backfill.

## The actual cutover risk

It is DNS, and it is one-way. A custom domain points at Statuspage by CNAME and
can point at only one provider at a time, so the moment you switch it every
existing bookmark, email footer, support macro and uptime checker follows. Bring
the ilert page fully live on its `subdomain` first, verify it with real traffic,
import the subscribers, and switch the CNAME only when the page is worth arriving
at and the people who care are already on it.

Run the two in parallel while that is true. Post to both during the overlap: a
status page that is stale during its own migration costs more trust than the
migration saves, and it is the one system whose failures your customers see
before you do.

## A word on IaC

A migration might be the right choice to introduce IaC along the way for most resources.
If that is a desired choice ilert offers an official Terraform provider https://registry.terraform.io/providers/iLert/ilert/
To which the same rules mentioned in this file may be applied.