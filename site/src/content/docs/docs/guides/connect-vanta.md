---
title: Connect Vanta
description: Create Vanta API credentials, connect them, and let sessions run your compliance program.
sidebar:
  order: 14
---

Connecting Vanta lets sessions read and update your compliance program through the Manage
Vanta API: people and their security tasks, automated tests and the entities failing them,
controls and frameworks, policies, evidence documents, issues, vendors and their reviews,
risk scenarios, vulnerabilities, connected integrations, Trust Center, and customer-trust
questionnaires. Vanta webhooks can also start automations.

Vanta issues a short-lived token from a client id and a client secret. engrams obtains the
token server-side and obtains a new one before it expires, so there is no consent screen and
nothing to renew by hand.

## Create the API credentials

1. In Vanta, open Settings → Developer Console and create an application of the Manage
   Vanta kind. Give it the `vanta-api.all:read` and `vanta-api.all:write` scopes. engrams
   asks for both; a token request for a scope the application lacks is refused by Vanta.
2. Generate a client secret and copy the client id and the secret.

Vanta keeps one active token per application. Use these credentials for engrams alone, or
each new token revokes the one the other consumer holds.

## Connect it

Open Settings → Integrations → Vanta → Connect, paste the client id and the client secret,
and select Add Vanta. engrams seals the two values as the org secrets `vanta.client_id` and
`vanta.client_secret`, obtains the token, seals it, and marks the integration connected.
Reconnect from the integration page obtains a fresh token from the sealed credentials, for
example after you rotate the secret and paste the new value.

The token never enters a sandbox. The egress proxy adds it to each request as a bearer token
and removes any token the agent supplies.

## What sessions get

A profile grants Vanta powers by area, each split into read and write: `people:read` and
`people:write`, `tests:read` and `tests:write`, `controls`, `frameworks`, `documents`,
`vendors`, `risks`, `vulnerabilities`, `integrations`, `trust-center`, and `customer-trust`.
`issues:read`, `policies:read`, and `organization:read` are read-only because the API offers
no writes there. `resources:write` lets a session push its own evidence to Vanta as a
custom integration.

The `vanta` command is on the PATH in every session whose profile grants at least one Vanta
power. It is a generic client over the whole API: every resource is reached through `list`,
`get`, `create`, `update`, and `delete`, `resources` prints the resource table with the verbs
each supports, and `api` reaches any other route, such as the action endpoints. Query fields
pass straight through as the API's filters, and `list` follows every page and prints one JSON
array. Run `vanta` with no arguments for the command list, or `engrams-integrations` for every
connected tool.

```bash
vanta list people tasksSummaryStatusMatchesAny=OVERDUE
vanta list people taskTypeMatchesAny=ACCEPT_POLICIES,COMPLETE_TRAININGS taskStatusMatchesAny=DUE_SOON
vanta list tests statusFilter=NEEDS_ATTENTION
vanta list issues statusMatchesAny=NOT_STARTED,IN_PROGRESS dueBeforeDate=2026-11-15T23:59:59Z
vanta get people <id>
vanta api POST controls/<id>/set-owner '{"userId":"<user-id>"}'
```

## Receive Vanta events

Vanta sends webhooks for questionnaires, Trust Center access requests, vendor decisions,
and, for auditor accounts, information requests, evidence, and control comments. Vanta
sends no webhook when a test fails or a person's task becomes overdue; an automation on a
schedule reads those with the commands above.

1. In the Connect sheet, or later under Settings → Secrets, paste the endpoint's signing
   secret as `vanta.webhook_secret`. The secret starts with `whsec_` and is shown in Vanta
   under Settings → Webhooks after you add the endpoint.
2. In Vanta, add the endpoint and choose the events you want:

   ```
   <origin>/api/v1/integrations/vanta/events
   ```

Each delivery is verified against the signing secret, and a delivery older than five minutes
is refused. The events then appear as triggers under Automations.
