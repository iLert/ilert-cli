---
name: setup-environment-profile
description: Point the ilert CLI at a staging, preview or self-hosted ilert environment by giving it its own profile
user-invocable: true
---

# Setting up a profile for another ilert environment

Use this when the CLI has to talk to something other than ilert's production API —
a staging or preview instance, or a self-hosted deployment.

Nothing about such an environment is compiled into the CLI. The endpoint
and the OAuth application that goes with it live in the operator's own config, so
setting one up is a configuration task, not a code change. **Never add a hostname
or client id to this repository** — not to source, not to a test, not to this
document.

## What a profile is

A profile is a named bundle of *where* and *as whom*, stored in `config.json`
(`ilert config show` prints the path):

| Setting | Flag | Environment variable |
| --- | --- | --- |
| `base_url` | `--base-url` | `ILERT_BASE_URL` |
| `oauth_client_id` | `--oauth-client-id` | `ILERT_OAUTH_CLIENT_ID` |
| `team_context` | `--team-context` | `ILERT_TEAM_CONTEXT` |

Each resolves flag → environment → profile → built-in default, and the built-in
default is production. Credentials never go in that file; they go to the OS
keyring under the profile's name. The cached API spec is also per environment, so
`ilert --help` under one profile lists that environment's commands and cannot be
polluted by another's.

`--profile NAME` (or `ILERT_PROFILE`) selects one and works anywhere on the
command line. **Switching environments is what `--profile` is for.** A credential
is bound to the endpoint that issued it, so `--base-url` on an ordinary command
does not borrow another profile's login — it is refused (see below). `--base-url`
belongs on `auth login`, where a credential for that environment is obtained.

## What you need before you start

Two values, both specific to the target environment:

1. **The API base URL** — the API host, not the web app.
2. **The OAuth client id** of the CLI application *registered on that
   environment*. A client id is only valid at the endpoint it was registered
   with; production's will be rejected.

**Ask the user for both.** Do not guess a hostname from a pattern, do not reuse a
value from another environment, and do not go looking for them in the codebase —
they are deliberately not there. They may already be exported as `ILERT_BASE_URL`
and `ILERT_OAUTH_CLIENT_ID`, which is worth checking first.

If the environment is reachable with an **API key**, you need only the base URL —
the client id is an OAuth-only concern. Prefer that path when there is no browser.

## Set it up

Pick a profile name that says which environment it is (`staging`, `preview`,
`selfhosted`). Then, once:

```bash
ilert --profile <name> auth login \
  --base-url <api-base-url> \
  --oauth-client-id <client-id>
```

That opens a browser, stores the credential in the keyring under `<name>`, writes
the two settings into the profile, and fetches that environment's spec. Every
later command needs the profile only:

```bash
ilert --profile <name> alerts list
ilert --profile <name> ops list
```

Headless, or an API-key environment:

```bash
echo "$KEY" | ilert --profile <name> auth login --base-url <api-base-url> --with-token
```

CI, where the values arrive as secrets and no `config.json` is checked out — the
environment variables are enough on their own, no login step:

```bash
export ILERT_BASE_URL=... ILERT_API_KEY=...
ilert alerts list
```

Export the two **together**. A key that arrives through `ILERT_API_KEY` is bound
to the endpoint the environment itself names — `ILERT_BASE_URL`, else the profile
`ILERT_PROFILE` selects, else production — so `ILERT_API_KEY` alone plus a
`--base-url` or `--profile` on the command line is refused rather than sent.
Select the profile with `ILERT_PROFILE` when the key comes from the environment
too, and both are chosen the same way. Only a key typed on the command line with
`--api-key` may target an endpoint of its own.

For OAuth in CI, export `ILERT_OAUTH_CLIENT_ID` as well so the silent token
refresh authenticates as the right application. `ilert config import` turns the
exported `ILERT_*` variables into a stored profile if you want one.

## Verify

```bash
ilert --profile <name> auth show     # base_url, credential_endpoint, oauth_client_id
ilert --profile <name> auth whoami   # proves the token works against that endpoint
ilert config list                    # every profile, and which is default
```

`auth show` is the fastest way to confirm a command is really pointed where you
think — check `base_url` before concluding anything is broken. `credential_endpoint`
next to it is where the stored credential was issued; the two matching is what
lets the command run.

## When it does not work

**`invalid_client` / the authorize page rejects the login.** The client id is not
registered on that endpoint. Almost always production's id against another
environment, or the reverse.

**"Refusing to send profile '&lt;name&gt;' credentials to &lt;url&gt; — they were issued
for &lt;other-url&gt;".** Working as intended: a `--base-url` (or `ILERT_BASE_URL`)
override pointed a profile's stored credential at an environment it was not
issued for, and the CLI will not hand it over. The fix is a profile of its own —
`ilert --profile <name> auth login --base-url <url>` — then select it with
`--profile`. For a one-off, pass `--api-key`, which targets whatever endpoint you
name. Logout and the silent token refresh are held to the same rule, and a logout
revokes at the issuing endpoint rather than the overridden one.

**"Refusing to send the API key from ILERT_API_KEY".** Same rule, applied to an
exported key: it goes where the environment points, not where a flag does — and
`--profile` is a flag, so it cannot pick the environment the key is credited to
either. Export `ILERT_BASE_URL` (or `ILERT_PROFILE`) alongside the key, or pass
the key as `--api-key`.

**The browser redirect never lands.** The CLI listens on a fixed loopback port and
the environment's OAuth application must have that exact redirect URI registered.
The port is printed in the error; free it, or fall back to `--with-token`.

**"Refusing to send credentials over cleartext HTTP".** The base URL must be
`https://`, except on loopback. `ILERT_ALLOW_INSECURE_HTTP=1` exists but do not
reach for it against a remote host — a plain-HTTP endpoint is the bug.

**The wrong commands show up in `--help`.** The spec cache is keyed by base URL,
so this means the command ran against a different environment than intended.
Re-check with `auth show`.

**A command ignores the profile.** Something higher in the chain is winning — a
`--base-url` flag, or an exported `ILERT_BASE_URL` / `ILERT_API_KEY` left over in
the shell. Environment variables beat the stored profile by design. If the command
authenticates, this usually surfaces as the refusal above rather than as a silent
misroute.

## Leave the repository alone

The profile lives in the user's config directory and the credential in their
keyring. Setting one up should produce **no diff**. If you find yourself editing a
tracked file to make another environment work, stop — that is the thing this
design exists to avoid.
