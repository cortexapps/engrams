---
title: Connect Linear
description: Create the Linear OAuth application, connect it, and trigger automations from issues.
sidebar:
  order: 12
---

Connecting Linear lets sessions read and write issues, projects, and comments through
Linear's API, and lets automations trigger when an issue or comment is created or updated.

## Create the OAuth application

1. In Linear, open Settings → API → OAuth applications and create one.
2. Set its redirect URI to `<origin>/api/v1/integrations/linear/oauth/callback`, where the
   origin is your deployment's public URL.
3. In the application's webhook settings, set the URL to
   `<origin>/api/v1/integrations/linear/events` and choose a signing secret. Do this before
   anyone authorizes: Linear provisions the webhook for a workspace at authorization time,
   and an installation that predates the webhook settings silently has none. If that happens,
   Reconnect from the integration page provisions it.
4. Copy the client ID and the client secret.

## Connect it

Open Settings → Integrations → Linear → Connect, paste the client ID and secret, and
select Add Linear. engrams sends you through Linear's consent screen, requesting the `read`
and `write` scopes as the application itself, and returns to the integration page marked
connected. Add the webhook signing secret as the org secret `linear.webhook_secret` under
Settings → Secrets.

Webhook deliveries are verified by signature and rejected when their timestamp is more than
a minute old.

## What sessions and automations get

Sessions on a profile that grants Linear powers can query and mutate issues through the
organization's connection; the token is brokered at the egress proxy and never seen by the
agent. Automations can trigger on `issue.create`, `issue.update`, and `comment.create`,
scoped by team key, and can create issues and comments as actions.

## Personal Linear credentials

Under Settings → Credentials a person can connect their own Linear account by OAuth or paste
a personal API key. A profile set to act as the launching person then posts as that person
rather than as the application.
