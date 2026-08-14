---
name: integrations
description: Third-party integration CLIs (e.g. gh for GitHub, glab for GitLab, stripe for Stripe, pup and datadog for Datadog, linear for Linear, slack for Slack, gcloud for Google Cloud) are available in this session, authenticated automatically by the engrams platform. Use when you need to interact with a connected service from the command line. Run `engrams-integrations` to see which are enabled and how to use each.
---

# Integration CLIs

This session has command-line tools for the third-party services your profile
connected — for example `gh` (GitHub), `glab` (GitLab), `stripe` (Stripe),
`pup` and `datadog` (Datadog), `linear` (Linear), `slack` (Slack), and
`gcloud` (Google Cloud). **You never
handle real credentials**: each tool carries a harmless placeholder token, and the
engrams egress proxy injects the real, capability-scoped credential on the wire.
Do not paste, export, or `login` with real API tokens — it's already wired.

## See what's enabled (and how to use it)

```bash
engrams-integrations
```

This prints the integrations authenticated **in this session** and the
per-tool usage notes. Only the services your profile enabled will authenticate;
calls to anything else are denied at the network boundary.

## How auth works (so you don't fight it)

- Each CLI already has a placeholder credential in its environment (e.g.
  `GH_TOKEN=x-engrams-managed`). **Leave it as-is** — the platform replaces it
  host-side on every request. Re-`login` or overwriting it will not help and may
  break the brokering.
- Requests only reach the hosts your enabled integrations allow; everything else
  is blocked. A tool whose integration you didn't enable will fail at the network
  boundary, not because it's missing.
- There is nothing to log in to, no token to paste, no `*_API_KEY` to set.
- `gcloud` reads its credential from a session-local metadata endpoint, the same
  way it would on a Compute Engine instance. Do not run `gcloud auth login`, do
  not create an application-default-credentials file, and do not copy a service
  account key in — all three are refused, and none is needed. The metadata
  endpoint holds no project, so pass `--project` (or set
  `CLOUDSDK_CORE_PROJECT`) when a command needs one.

## Cloud SQL PostgreSQL

If `engrams-integrations` lists Cloud SQL, inspect the session-local tunnel
names. They are connection aliases, not network destinations:

```bash
engram-tunnel list
```

Then expose the selected tunnel on guest loopback:

```bash
engram-tunnel open CONNECTION_ALIAS --port 5445
```

Then connect with the IAM database user. The loopback leg does not use TLS; the
host encrypts the upstream leg:

```bash
PGSSLMODE=disable PGAPPNAME="engrams-$ENGRAM_SESSION_ID" \
  psql --host 127.0.0.1 --port 5445 --username SERVICE_ACCOUNT_NAME@PROJECT_ID.iam --dbname DATABASE
```

Do not supply a password. The loopback leg is local to the guest; the host Cloud
SQL Auth Proxy encrypts the upstream leg with TLS. The host performs automatic
IAM authentication. The
instance must have `cloudsql.iam_authentication=on`. The database role controls
whether the session is read-only; do not rely on SQL
text inspection. Prefer a role with only `CONNECT`, schema `USAGE`, and table
`SELECT`, plus `default_transaction_read_only=on` and a statement timeout.
