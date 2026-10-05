---
title: Connect Cortex
description: Pick the Cortex API host, add a workspace API key, and give sessions the catalog.
sidebar:
  order: 13
---

Connecting Cortex lets sessions read and update your internal developer portal through
Cortex's REST API: catalog entities, scorecards and scores, teams and on-call, custom data,
deploys, dependencies, and CQL queries. engrams is built by Cortex, so this integration ships
with the product and sits first in the integrations list.

## Pick the API host

Cortex Cloud runs in two regions, and some customers run their own instance. Open
Settings → Integrations → Cortex → Connect. The first field is the API host:

- **Cortex Cloud, US**: `api.getcortexapp.com`. This is the default.
- **Cortex Cloud, EU**: `api.eu.cortex.io`.
- **Self-hosted**: type the hostname your Cortex API answers on, such as
  `cortex-api.example.com`. Enter a bare hostname, without a scheme, port, or path.

Sessions can reach the host you pick and no other Cortex host. Change it later from the
integration page; the change applies to the next session that starts.

## Add a workspace API key

1. In Cortex, open your avatar → Settings → API keys and create a key. Give it only the
   permissions the sessions need, and set an expiration.
2. Paste the key into the Connect sheet. engrams seals it as the org secret `cortex.api_key`
   and tests it with one read against the host you picked.

The key never enters a sandbox. The egress proxy adds it to each request as a bearer token
and removes any token the agent supplies.

## What sessions get

A profile grants Cortex powers by area: `catalog:read` covers entities and everything
attached to them, and each write is its own power, such as `custom-data:write` or
`deploys:write`. The `cortex` command is on the PATH in every session whose profile grants
at least one Cortex power. Run `cortex` with no arguments for the command list, or
`engrams-integrations` for every connected tool.

```bash
cortex search payments
cortex entity payments-api
cortex scores production-readiness payments-api
cortex set-custom-data payments-api last-audit '{"date":"2026-09-21"}'
cortex query 'entity.type = "service" AND scorecard("production-readiness").score < 50'
```

Entity and team deletion, bulk deletes, API key management, secrets, the IP allowlist,
and the integration-configuration endpoints are not reachable through this connector.

## Personal Cortex tokens

Under Settings → Credentials a person can paste a personal access token from their Cortex
avatar → Settings → Preferences → My access tokens. A profile set to act as the launching
person then calls Cortex with that person's own permissions instead of the workspace key.
Personal tokens use the same API host the administrator picked.
