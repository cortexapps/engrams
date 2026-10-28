---
title: Connect Rippling
description: Create a Rippling API token, connect it, and give sessions your org chart and HR records.
sidebar:
  order: 15
---

Connecting Rippling lets sessions read your HR system through the Rippling REST API: workers
and their managers, users, departments, teams, locations, titles and levels, employment
types, legal entities, leave, time, devices, worker changes, the activity log, custom
objects, and reports, plus the writes Rippling exposes on those records.

## Create an API token

Rippling's REST API is part of the Rippling API product; the token is created by an admin.

1. In Rippling, open the API credentials app (Settings → Developer → API credentials) and
   create a new token. Choose the scopes the sessions need; each read command below names
   the scope family it uses, such as `workers.read` or `departments.read`.
2. Copy the token. Rippling revokes a token that is unused for 30 days or whose owner leaves,
   so create it under a service admin where you can.

## Connect it

Open Settings → Integrations → Rippling → Connect and paste the token. engrams seals it as
the org secret `rippling.api_token` and tests it with one read of the company record.

The token never enters a sandbox. The egress proxy adds it to each request as a bearer token
and removes any token the agent supplies. Fields the token is not entitled to read come
back redacted, listed under `__meta.redacted_fields` in each response.

## What sessions get

A profile grants Rippling powers by area, each split into read and write: `workers:read`,
`org:read` and `org:write` (departments, teams, locations, titles, levels, job catalog),
`hiring`, `compensation`, `payroll`, `leave`, `time`, `devices`, `custom-objects`, `reports`,
`files`, `workflows`, `activity-log:read`, and `company:read`. Workers, users, and the
activity log are read-only because the API offers no writes there.

The `rippling` command is on the PATH in every session whose profile grants at least one
Rippling power. It is a generic client over the whole API: every resource is reached through
`list`, `get`, `create`, `update`, and `delete`, `resources` prints the resource table with
the verbs each supports, and `api` reaches any other route. Query fields pass straight
through in Rippling's own `filter`, `expand`, and `order_by` syntax, and `list` follows every
page and prints one JSON array. Run `rippling` with no arguments for the command list, or
`engrams-integrations` for every connected tool.

```bash
rippling list workers "filter=work_email eq 'someone@example.com'" expand=user,manager
rippling list workers "filter=manager_id eq '<worker-id>'" expand=user
rippling list worker-changes "filter=updated_at gt '2026-10-01T00:00:00Z'"
rippling get departments <id>
rippling create leave-requests '{"worker_id":"<id>","leave_type_id":"<id>","start_date":"2026-11-02","end_date":"2026-11-03"}'
rippling api POST custom-objects/<api-name>/records/query '{"query":"SELECT * FROM ..."}'
```

Rippling sends customer webhooks only for custom-object records, so an automation that
reacts to HR changes runs on a schedule and reads `rippling list worker-changes` for the window
since its last run.

The developer-platform surfaces (apps, functions, packages, sandboxes, listings, and sites)
are not reachable through this connector.
