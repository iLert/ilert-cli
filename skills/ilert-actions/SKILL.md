---
name: ilert-actions
description: Build, customize and debug ilert actions (outbound integrations): templates, credentials, custom HTTP steps, bidirectional callbacks
---

# ilert actions: outbound integrations

An **action** calls another system when an alert or incident changes: it opens a
Jira issue, posts to a webhook, sends an email or reroutes the alert. Actions
are the successor to alert actions + connectors. Use this skill for any
outbound integration work, whether it is set up from a ready-made template or
written from scratch as custom HTTP steps. It also says when to use alert
actions instead.

Read `ilert-essentials` first.

## First: actions or alert actions?

Actions are in **closed beta**, and ilert enables them account by account. An
account **with** actions should use them, but alert actions keep working
there too, and some integrations still require them.

**1. Does the account have actions?** Check this before planning anything:

```sh
ilert actions list --max-results 1
```

A `404` with a "not available" message means it does not, so use
`ilert alert-actions` with `ilert connectors`. Branch on that `404`, not on an
empty list, because an enabled account with no actions returns `[]`.

**2. Does this job fit actions?** Prefer actions, except in these cases:

| Situation | Use |
| --- | --- |
| The integration exists only as an alert-action type, with no action template (`ilert actions list-templates`). Today that includes Slack, Microsoft Teams, Google Chat, Zoom Chat, DingTalk, Zabbix and automation rules. Compare the live template brands with the stable spec's `AlertActionType` rather than relying on this list | **Alert actions**. Slack and Microsoft Teams are ChatOps integrations with their own setup in the web app |
| Changing an existing alert action | **Alert actions** (`ilert alert-actions update`). Its mirror under `/actions` is read-only |

Both kinds run side by side, and names are unique across both. In
`ilert actions list`, alert actions appear as read-only mirrors with
`origin: LEGACY_REF`. **Deleting a mirror deletes the real alert action.**
Rebuilding an alert action as an action is a migration: create the action,
verify it in its logs, and only then delete the alert action, with the human's
consent. Until then both fire.

## The pieces

| Piece | What it is |
| --- | --- |
| **Template** | A vendor integration ilert maintains (`jira_cloud_alert`, `webhook_incident`, …), versioned. It defines the `steps`, the `config` form (`configSpec`), the accepted credential kinds (`authSpec`) and, if bidirectional, the `inbound` handling |
| **Action** | Binds one template (or its own `steps`) to one `entityKind` (`ALERT` or `INCIDENT`, fixed after creation), with trigger events, an ICL condition, `config` and a `credentialId` |
| **Credential** | The vendor URL and secret (`ilert credentials`, see `ilert-essentials`). One credential can serve several actions |
| **Twin** | The link between one alert/incident and the object an action created for it (the Jira issue, the ticket). Later runs update that object instead of creating a new one, and callbacks find their alert through it |
| **Activation** | The record that an action was run manually on one entity. From then on it follows that entity's lifecycle |

## Pick the path

**Default to the template as it is.** Most users want a template's standard
behaviour. A plain template action also picks up ilert's fixes and updates to
the template, and leaves nothing custom for the customer to maintain.
Customize only when the human asks for
behaviour the template's `config` cannot express.

| The user wants | Path |
| --- | --- |
| A supported vendor with its standard behaviour (the usual case) | **Template as is**: `templateKey` + `config` + `credentialId`. Leave `steps`, `inbound` and `fieldMapping` out entirely |
| A supported vendor that should behave differently | **Template + own `steps` and/or `inbound`**: start from the template's full definition (`--full true`) and change it. Your `steps` replace the template's completely |

**Never send a copy of the template's own `steps`, `inbound` or
`fieldMapping`.** ilert does not compare them with the template: whatever you
send is saved as the action's own definition and runs instead of the
template's. A verbatim copy validates and works today, but it is frozen. It
no longer picks up ilert's changes to the template, and `templateVersion: null`
stops applying to it. Send these fields only with real changes the human
asked for.
| A system with no template: an internal API, a ticketing tool, a runbook endpoint | **No template**: the action defines its own `steps`, over HTTP |

Actions are one-way by default, and that is what most users need. Set up
callbacks only when the human asks for changes in the vendor to flow back into
ilert (see the last part of this page).

Templates whose `transport` is not `HTTP` (email, Telegram, EventBridge,
reroute, set-service-status) take `config` only, not `steps`.

## Workflow

```sh
# 1. What exists. Do not hard-code the list, it grows.
ilert actions list-templates --jq '.templates[] | {key, brand, transport, bidirectional, auth: [.authSpec[].kind]}'

# 2. What a template needs. This is all a template-as-is action uses.
ilert actions get-templates --key jira_cloud_alert --jq '{config, configSpec, authSpec}'
#    Only when customizing, as the starting point for your own steps:
#    ilert actions get-templates --key jira_cloud_alert --full true --jq '{steps, inbound, fieldMapping}'

# 3. A credential: reuse one, or create one (see Credentials)
ilert credentials list --jq '.[] | {id, name, typeHint, subCategory}'

# 4. Check the credential against the vendor, and get values for select fields
ilert actions get-options --key jira_cloud_alert --field project --credentialId 123
ilert actions get-options --key jira_cloud_alert --field issueType --credentialId 123 \
  --config '{"project":"OPS"}'          # fields in dependsOn must be in config

# 5. Validate, then save
ilert actions create-templates-validate --body-file draft.json
ilert actions create-validate --body-file condition.json
ilert actions create --body-file action.json

# 6. Prove it ran
ilert actions list-logs --id <ACTION_ID> --max-results 10
```

The CLI names come from the paths: `get-templates` reads **one** template,
`create-templates-validate` checks a **draft** and `create-validate` checks a
**condition**.

There are three checks. None of them saves anything, and each covers
something the others do not:

| Check | What it proves |
| --- | --- |
| `get-options` | **The credential really works**: it is the only check that calls the vendor. It exists only for template fields that load their options from the vendor |
| `create-templates-validate` | The definition is executable: steps, `config` against `configSpec`, field mappings, inbound, and whether the credential's *kind* suits the template. It cannot tell whether the secret is right |
| `create-validate` | The trigger condition, as the calling user |

`create` and `update` run the same checks, so iterate with the validators.
The draft check names fields differently from the action:

| Action | Draft (`create-templates-validate`) |
| --- | --- |
| `templateKey` | `template` |
| `conditionICL` | `trigger.conditionICL`, not checked here |
| `credentialId` | `credential: {kind, hostUrl}`, where `kind` is the credential's `typeHint`. Sending `typeHint` instead is silently ignored, and the credential check is skipped while the draft still reports `valid` |
| (none) | `externalSystem`: any non-empty value such as `"custom"`. A template-less draft needs it for `referenceWrite` to be checked |

After `create`, read the action back: some templates register things in the
vendor and write the results into `config`. For a template-as-is action,
also check that it stores no definition of its own:

```sh
ilert actions get --id <ACTION_ID> --jq '{steps, inbound, fieldMapping}'
```

All three must be `null`, since a read returns only what the action itself
stores. If any of them is set, clear it with `"steps": []`, `"inbound": {}` or
`"fieldMapping": []` (see Updating).

## Config

`configSpec` is the form for the action's `config`, with one entry per key:

| `configSpec` entry | Meaning |
| --- | --- |
| `key` | The `config` key it writes |
| `type` | The shape of the value (see below) |
| `required` | Must be set, unless `visibleWhen` hides the field |
| `default` | What the web app prefills. **It is not applied at run time.** The validator still counts it as meeting `required`, so write the value into `config` yourself, as the web app does. Only keys in the template's own `config` fill in for missing ones |
| `selectOptions` | The allowed values: `key` is stored, `label` is shown |
| `fetch` | The options come from the vendor instead: read them with `get-options`, after setting every field in `dependsOn` |
| `visibleWhen: {field, in}` | Shown, required and checked only while `field` holds one of the values in `in` |
| `group` | The key of the `switchableGroup` field this one sits under |
| `templated` | The value may contain `{{ }}`, rendered against the alert. A `type: template` field always may |
| `triggerEvent` | A checkbox that, while ticked, also subscribes the action to that event |
| `readOnly` | Set by ilert. Never write it |

| `type` | Value in `config` |
| --- | --- |
| `text`, `password`, `select`, `template` | string |
| `number`, `long` | number |
| `ilertEntity` | number: the id of the ilert entity named by `entityType` |
| `checkbox`, `switchableGroup` | boolean |
| `chips`, `slist`, `ilist`, `keyValue` | array, even for a single value |
| `decisionTable` | array of `{conditionICL, outcome}`, first match wins, at most 50 rules, each `outcome` one of `selectOptions` |

A key that `configSpec` does not declare (a typo) gets only a validator
**warning**, and nothing reads it. Treat that warning as an error.

## Credentials

Each `authSpec` entry of a template is one accepted credential kind and lists
its form fields. They map onto the credential body like this:

| `authSpec` | Credential body |
| --- | --- |
| `kind` | `typeHint` |
| field `password` or `secret` | `secretValue` |
| fields `hostUrl`, `username`, `apiKeyHeaderName`, `idpUrl`, `idpClient`, `idpScopes`, and `oauthMethod` | same names |
| `compose` | `compose`, required when present (e.g. Zammad). The beta spec omits the field but the API accepts it |
| a field with `target: CONFIG` | the action's `config`, not the credential |

- **`NONE`** (webhook URLs: Discord, Mattermost, Zapier, a plain webhook):
  put the *whole* URL in `hostUrl` and send no `secretValue`. ilert stores the
  path as the secret, so a read shows only the origin. In steps,
  `{{ credential.hostUrl }}` renders the full URL again.
- To rotate one, create a new credential, point each action at it, then
  delete the old one. Find its users first with
  `ilert actions list --all --jq '.[] | select(.credentialId == <ID>) | {id, name}'`.
- Leave `category: "MCP"` credentials alone. Action credentials use
  `category: "global-action"` with the template `brand` as `subCategory`, as
  the web app does.
- Never ask for a secret in chat or put one in argv. Have the human export it
  and build the body from the environment:

```sh
jq -n --arg secret "$JIRA_API_TOKEN" --arg user "$JIRA_EMAIL" '{
  name: "Jira OPS", typeHint: "BASIC", hostUrl: "https://acme.atlassian.net",
  username: $user, secretValue: $secret,
  category: "global-action", subCategory: "jira" }' \
| ilert credentials create --body - --jq '{id, name}'
```

## Writing steps

The JSON the API takes has the same structure as the YAML the web app shows
under **Action definition**.

```json
{ "id": "open-ticket", "when": "twin == nil", "operation": "CREATE",
  "requests": [{ "id": "post", "method": "POST",
    "url": "{{ credential.hostUrl }}/api/tickets",
    "headers": { "Content-Type": "application/json" },
    "body": "{ \"title\": \"{{ entity.summary.orElse(\"ilert alert\").truncate(200) }}\" }",
    "expect": { "successStatus": [200, 201] }, "retry": "NEVER" }],
  "referenceWrite": { "externalId": "{{ last.body.id }}",
    "externalUrl": "{{ credential.hostUrl }}/tickets/{{ last.body.id }}",
    "ref": { "ticketId": "{{ last.body.id }}" } } }
```

- **Every step whose `when` is true runs**, in order unless `needs` reorders
  it. It is not an if/else chain, so make conditions exclusive yourself.
- **The twin is the idempotency mechanism.** Before the first `CREATE` step
  succeeds, `twin == nil`. A `CREATE` step must write
  `referenceWrite.externalId`, and later runs read `twin.ref.<key>`.
- **Failures are final.** A response outside `expect` fails the run, and it is
  not retried later; only the request's `retry` applies. `AUTO` always retries
  `GET`/`PUT`/`DELETE`/`HEAD`, and other methods only once a twin exists. Use `NEVER`
  for anything that must not happen twice, such as a comment. Use
  `expect.condition` (`last.body.ok == true`) for APIs that report errors with
  a `2xx`.
- **The credential adds the auth header**, so never set one yourself.
  `{{ credential.hostUrl }}` works only at the very start of a `url`.
- **Absent, not null.** Missing fields are left out: test with `!= nil` and
  render with `.orElse("…")`.
- **`config` values are strings in ICL** (`config.pushComments == 'true'`),
  but are saved typed per `configSpec` (see Config): a checkbox takes a JSON boolean.
- With `Content-Type: application/json`, values rendered into `body` are
  JSON-escaped for you. Write `"{{ entity.summary }}"` as is.
- To avoid escaping JSON inside JSON by hand, keep a long body in its own file
  and embed it with `jq -n --rawfile body body.tmpl '{… body: $body …}'`.

| Root | Holds |
| --- | --- |
| `entity` | The alert or incident *after* the change, plus event-only fields such as `comment` or `cause` |
| `signal` | `type` (`alert.created`, …), `occurredAt`, `dispatch`, and `origin.system` on synced-back comments |
| `actor` | Who made the change. Absent on escalations, priority raises and delayed events |
| `execution.correlationId` | This run. Use it as an idempotency key toward the vendor and to find the run in the logs |
| `config`, `tenant`, `vendor` | Settings, the account, vendor constants |
| `twin`, `last`, `responses.<id>`, `steps.<id>` | The stored link, and the responses so far |
| `map.<name>` | Field-mapping values a template publishes |

Fields per root: <https://docs.ilert.com/integrations/actions/context.md>.
Fetch the page you need rather than guessing.

## Triggers and conditions

- `dispatchMode`: `AUTOMATIC` runs on matching events. `MANUAL` runs only when
  someone runs it on an entity that matches the condition.
- `triggerEvents`: empty means **every** event of the kind.
- For "still open after N minutes", use the delayed `alert.not_resolved` /
  `incident.not_resolved` events, not escalation: accepting an alert stops
  escalation.

### The condition is also the permission scope

`conditionICL` decides which entities an action runs for, and with that,
whose alerts or incidents it acts on. ilert checks it against the user who
saves the action:

- **Non-admins** must restrict **every `or` branch** to entities they can
  reach. For alerts, that means their teams (`entity.teamIds`) or alert sources
  they can read (`entity.source.id`). For incidents, it means their teams or
  services they can read (`entity.serviceIds`). An extra filter such as labels
  or priority narrows a restricted branch, but it cannot replace the
  restriction: `entity.teamIds contains_any ["12"] or entity.priority == "HIGH"`
  is refused. Non-admins can name at most 50 ids.
- The web app builds this for the user. Its **Run this action for** choice is
  *All alert sources* (admins only), *My teams* or *Specific sources* (for
  incidents, *Specific services*). It writes the choice as the first filter of
  every filter group, e.g. `entity.source.id in ["1234"]`, with ids as strings.
  The user's own filters are ANDed after it. Build conditions the same way.
- Check a condition with `create-validate` before saving.

### An empty condition means every alert or every incident

An admin may leave `conditionICL` empty, or use a template whose condition
does not filter. The action then runs on **every** alert, or every incident, in
the whole account, including those from sources and services created later.
Only one enabled `AUTOMATIC` action per account and entity kind may run
without a condition, and the validator names the one that already does.

Treat this with the utmost care. Create an account-wide action only when the
human explicitly asks for it and has confirmed that "every alert in the
account" is what they mean. In every other case, scope it. If the slot is
taken, add a condition to the new action. Do not disable or change someone
else's account-wide action to free the slot.

## Bidirectional actions (callbacks)

This is optional. Set it up only when the human asks for it. It needs a
template with `bidirectional: true`, and template-less actions cannot use it.

1. Use a template with `bidirectional: true` and set `bidirectional: true` on
   the action.
2. Read back `integrationUrl`. It contains a secret: hand it to the human for
   the vendor's webhook settings, and keep it out of tickets and logs.
3. Callbacks are matched through the twin, so a callback changes only an
   entity that this action created a vendor object for.

The vendor needs only the action's `integrationUrl`, never an alert source
key. For each callback, ilert finds the alert or incident through the twin and
applies the verb to whatever it found. So one bidirectional action covers
alerts from every source its condition matches.

For an alert, `ACCEPT`, `RESOLVE`, `COMMENT` and `REROUTE` are applied as an
ordinary event on **that alert's own alert source**. As a result, **the
source's event filters can drop a vendor-originated change**: if a callback
did nothing, check that alert source's filters. Accepting an alert that is
already accepted, or resolving one that is already resolved, is a no-op.
`REROUTE` is refused on a resolved alert, and it swaps the whole escalation
policy.

**Creating new alerts from vendor events is not supported yet.** A new alert
would need an alert source to land in, and actions cannot name one so far.
`CREATE` is accepted in `classify` but dropped, so callbacks about objects
ilert did not create change nothing.

To change the inbound behaviour, send `inbound`. It replaces the template's
block **as a whole**, so start from `--full true`. `classify` is
first-match-wins: end it with a rule that has no `when` and `then: DROP`.
`fields` carries the values for the verb: `comment` (for `COMMENT`, and as a
note on `ACCEPT` / `RESOLVE`), `message` (a fallback for `comment`) and
`routingKey` (for `REROUTE`). Inbound templates see `payload`, `config`,
`vendor`, `map` and the `hydrate` reads, not `entity`, `twin` or `signal`.

Keep `inbound.matchBy.externalSystem` as the template's `brand`, written as a
plain string. ilert stores every twin under the brand, so a callback that
matches on any other system finds no twin and changes nothing. The validator
rejects a different plain value, but a templated one (`{{ … }}`) is only
checked when a callback arrives.

**Echo loops.** A comment ilert posts to the vendor comes back through the
callback as a new comment. Guard comment steps with
`signal.origin.system != '<brand>'`, and set `authoredId: "{{ last.body.id }}"`
on the step so `inbound.echoBy` drops the echo.

Rotating the integration key kills the old URL immediately. Get the new one
into the vendor right away.

## Updating

**`PUT /beta/actions/{id}` merges**, unlike the full-replace `PUT` elsewhere
in the API. Absent fields keep their value. To clear something, send it
empty: `"credentialId": null`, `"steps": []` (back to the template's),
`"inbound": {}` (back to the template's), `"fieldMapping": []` (back to the
template's). Send `config` as a complete object.
`templateVersion: null` follows the latest template version, and a number pins
one.

## Run and debug

```sh
ilert actions create-activations --id <ID> --body '{"entityType":"ALERT","entityId":<ALERT_ID>}'
ilert actions list-logs --id <ID> --status FAILED \
  --jq '.[] | {timestamp, triggerType, entityId, statusCode, failedStep, failedRequest, errorMessage}'
ilert actions get --id <ID> --jq '{status, lastError, lastErrorStepId}'
```

- **Running an action is real**: the ticket opens and the webhook fires. The
  action also stays attached to the entity, and its later changes trigger it
  again. Use a test alert. A `null` `correlationId` in the answer means the
  condition rejected the entity and nothing ran.
- Stop an attachment with `update-activations` (`{"enabled": false}`), or
  remove it with `delete-activations`.
- If the human wants to try an action before it goes live, one option is to
  create it as `MANUAL`, run it on a test alert, read the logs, then switch it
  to `AUTOMATIC`. This is optional, not a required step.
- Execution is asynchronous, so give the logs a few seconds.

## Pitfalls

- `brand` and `transport` on a saved action are often `null` or `HTTP`, even
  for email or Jira actions. Read the template to know what an action does.
- Telegram: a chat can belong to only one action or alert action per account.
