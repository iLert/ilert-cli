# Get ilert-ready

These are instructions for you, an AI coding agent, to set the human up on
ilert. Read them once, then act on them. The human who handed you this
document wants you to do the setup, not to explain it back to them.

ilert is an incident management platform: it turns events from monitoring
tools into alerts, pages whoever is on call until someone owns the alert, and
handles incidents and status pages from there. You work with it through one of
three surfaces, all using the same account:

- **The `ilert` CLI** is the main one. It builds every command from ilert's
  OpenAPI spec when it runs, so it covers the whole API, and it is designed to
  be used by agents: it never prompts when no human is attached, it prints JSON
  when piped, and its resource commands support `--dry-run`.
  https://github.com/iLert/ilert-cli
- **The ilert MCP server** at `https://mcp.ilert.com/mcp` is for the human's AI
  assistant (Claude, Cursor, and others), so it can query and act on alerts and
  incidents in conversation.
  https://docs.ilert.com/ai-and-agents/mcp/ilert-mcp-server-hosted.md
- **The ilert Terraform provider** is for teams that manage alert sources,
  escalation policies and schedules as code.

## 1. Decide which

Match what the human asked for:

| The human wants                                                                          | Do                  |
| ---------------------------------------------------------------------------------------- | ------------------- |
| To get started, or to have their app or monitoring page someone when it breaks           | Sections 2, then 3  |
| You to work their ilert account: check alerts, accept or resolve them, see who is on call, open an incident | Section 2, then 4 |
| To move over from another incident management or status page tool                        | Section 2, then 5   |
| Their ilert setup in Terraform, or existing resources exported to HCL                    | Section 2, then 5   |
| Their AI assistant or IDE connected to ilert                                             | Section 6           |

If it is unclear, ask one question: "Set up alerting for something, or work
with your existing ilert account?" Do not create resources the human did not
ask for.

## 2. Install the CLI and authenticate

macOS and Linux:

```sh
curl -sL https://raw.githubusercontent.com/iLert/ilert-cli/master/install.sh | bash -
```

The installer verifies the release checksum (and its release attestation, when
a compatible, authenticated GitHub CLI (`gh`) is available) before installing.
On Windows, download
`https://github.com/iLert/ilert-cli/releases/latest/download/ilert.exe` and put
it on the `PATH`. Anywhere with Docker: `docker run --rm ilert/ilert-cli`.

Check it with `ilert version`, then authenticate. Pick the path that matches
where you are running:

```sh
ilert auth login                                  # you run on the human's own machine
echo "$ILERT_API_KEY" | ilert auth login --with-token   # an API key, stored in the OS keyring
```

`ilert auth login` opens a browser for OAuth, or prints a URL if it cannot.
Show the URL to the human and wait for them to approve; do not try to work
around the approval step. The browser redirects back to `127.0.0.1` on this
machine, so **if you run in a sandbox, a container or a remote VM, OAuth will
not complete there**: use an API key instead. The human creates one in ilert
under **Profile → API Keys** (https://app.ilert.com).

Have the human configure credentials locally: set `ILERT_API_KEY` in your
environment, or run `ilert auth login --with-token` themselves. Never request
keys in chat. If `ILERT_API_KEY` is already in the environment (CI, or a key
the human made for you), skip the login: every command reads it. Never print
the key, write it to a file in the repository, or paste it into a command the
human will see in their history. Treat integration keys as secrets too.

No account yet? The human signs up at https://app.ilert.com/signup.

Confirm it worked:

```sh
ilert whoami        # the user, the account, and its plan
```

Then load the platform rules once. They explain the behaviour the field names
do not, and they will save you from the usual first mistakes:

```sh
ilert skills show ilert-essentials
```

## 3. Page someone with a real event

The path is **event → alert source → escalation policy → a person**. An alert
source receives events and owns the policy, the policy decides who is notified
and when. If the human already uses ilert, look first, because what they want
may already exist:

```sh
ilert alert-sources list
ilert escalation-policies list
```

Otherwise, find the human's user id, then create a policy and a source:

```sh
ilert whoami --jq .id

ilert escalation-policies create --body '{
  "name": "My first policy",
  "escalationRules": [{ "escalationTimeout": 15, "users": [{ "id": <USER_ID> }] }]
}'

ilert alert-sources create --body '{
  "name": "My app",
  "integrationType": "API",
  "escalationPolicy": { "id": <POLICY_ID> }
}' --jq .integrationKey
```

Add `--dry-run` to any of these to see the request without sending it.
`integrationType` is `API` for a generic HTTP source. If the events come from a
tool ilert integrates with (Prometheus, Grafana, Datadog, and many more), use
that tool's type instead: ilert then parses its payload and groups and resolves
alerts on its own. Find the integration at https://docs.ilert.com/llms.txt.

Fire a test event at the new source. This **pages the human for real**, so tell
them before you send it:

```sh
ilert event send -k <INTEGRATION_KEY> -s "Test alert from my agent" --alert-key test-1
ilert alerts list
ilert event send -k <INTEGRATION_KEY> -t RESOLVE -s "Resolved" --alert-key test-1
```

The event API answers `202` with an empty body: queued, not created. If no
alert appears, the reason is in the alert source's event log, not in the
response. https://docs.ilert.com/getting-started/quick-start-guide/troubleshooting.md

To send events from the human's code rather than the shell, `POST` JSON to
`https://api.ilert.com/api/events` with `integrationKey`, `eventType`
(`ALERT`, `ACCEPT`, `RESOLVE`, `COMMENT`), `summary`, and an `alertKey` so a
later `RESOLVE` finds the same alert. For a cron job or backup script that
should alert when it *stops* running, use a heartbeat monitor and
`ilert heartbeat ping <KEY>` instead.

## 4. Work the account

```sh
ilert alerts list                         # open alerts
ilert alerts ack <ID>                     # accept: a human owns it, escalation stops
ilert alerts resolve <ID>
ilert on-call now                         # who is on call right now
ilert incidents create --set summary="Checkout is down"
ilert status                              # an overview of the account
ilert --help                              # every resource and action
```

Anything the command tree does not cover, `ilert api /any/path` calls directly.

**Exit `2` means nobody consented, not that it failed.** Destructive commands
do not prompt without a human attached. They refuse and print a JSON envelope
that says what they would have done. Show the human, then rerun with `--yes`
only if they agree. Use `--dry-run` before any write you are unsure about: it
prints the same envelope and touches nothing.

Everything else about driving the CLI and the API (updates, paging, errors,
plan limits) is in `ilert-essentials` from section 2.

## 5. Migrate, or manage it as code

The CLI ships playbooks for these. List them and read the one that fits before
you start, after `ilert-essentials`:

```sh
ilert skills list                             # every playbook, with a description
ilert skills show <name>                      # e.g. a migrate-from-* playbook, or ilert-terraform
```

They map each source resource onto its ilert equivalent and list the order to
create things in. Order matters: an escalation policy imported before its
schedules exist looks correct and pages nobody.

To keep the skills in the human's agent setup permanently:
`npx skills add ilert/ilert-cli`.

## 6. Connect an AI assistant over MCP

The hosted server needs no installation, only an ilert API key (see section 2)
sent as a bearer token. For clients that support remote servers over
Streamable HTTP:

```json
{
  "mcpServers": {
    "ilert": {
      "type": "streamableHttp",
      "url": "https://mcp.ilert.com/mcp",
      "headers": { "Authorization": "Bearer <ILERT_API_KEY>" }
    }
  }
}
```

In Claude Code that is
`claude mcp add --scope user --transport http ilert https://mcp.ilert.com/mcp --header "Authorization: Bearer $ILERT_API_KEY"`.
Keep MCP configurations containing credentials out of version control.
Clients without remote server support, and the full list of tools, are covered
in https://docs.ilert.com/ai-and-agents/mcp/ilert-mcp-server-hosted.md.

## Read next

- Every documentation page, as Markdown: https://docs.ilert.com/llms.txt
- Product overview, with a curated selection of docs pages: https://www.ilert.com/llms.txt
