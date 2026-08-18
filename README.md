```
.__ .__                    __    
|__||  |    ____ _______ _/  |_  
|  ||  |  _/ __ \\_  __ \\   __\ 
|  ||  |__\  ___/ |  | \/ |  |   
|__||____/ \___  >|__|    |__|   
               \/                
```

The official [ilert](https://ilert.com) CLI. Every API endpoint, generated from ilert's
OpenAPI spec at runtime — so the command tree is never out of date.

## Install

### Quick install (MacOS / Linux)

```bash
curl -sL https://raw.githubusercontent.com/iLert/ilert-cli/master/install.sh | bash -
```

### Other installation options

```bash
docker run --rm ilert/ilert-cli   # from Docker Hub
cargo install --path .            # from source
# for cross-compilation see XCOMPILE.md
```

### Update

```bash
ilert update
```

Runs the installer that shipped inside the binary — same checksum and
attestation checks as a fresh install, and no script fetched from a branch —
and replaces the exact executable you invoked. It asks first, so add `--yes` to
update unattended.

## Use

```bash
ilert auth login                                # browser (OAuth)

ilert alerts list                               # list open alerts
ilert alerts ack 42                             # accept alert #42
ilert alerts list --all -o json                 # every page, as JSON
ilert incidents create --set summary=...        # create an incident
ilert event send -k <INTEGRATION_KEY> -s "msg"  # fire an event at an alert source
ilert status                                    # system overview
ilert dashboard                                 # live TUI
```

Headless or CI: set `ILERT_API_KEY` and every command picks it up — no login step.
To keep it in the OS keyring instead, pipe it in once with
`echo "$ILERT_API_KEY" | ilert auth login --with-token`.

Resources and actions come from the cached spec — `ilert --help` after login lists
them all, `ilert ops list` shows the raw operations, and `ilert api /any/path` hits
an endpoint directly.

## Flags worth knowing

| Flag | |
|---|---|
| `-o table\|json\|ndjson\|raw` | output format (JSON when piped) |
| `--jq EXPR` | filter JSON output |
| `--fields a,b` | pick table columns |
| `--dry-run` | print the request, send nothing |
| `-y` | skip confirmation prompts |
| `--profile NAME` | switch config profile |
| `--base-url URL` | point at another ilert environment (on `auth login`) |

## Agents

Destructive commands never prompt when no human is attached — they exit `2` with a
JSON envelope describing what they refused. `--dry-run` prints that same envelope
without touching the network or the keyring. `ilert skills list` / `ilert skills show
<name>` print the bundled migration playbooks or other skills as markdown.

## License

Apache-2.0
