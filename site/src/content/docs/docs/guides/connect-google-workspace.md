---
title: Connect Google Workspace
description: Give sessions your Google Workspace directory as the organization, and each member their own Gmail, Calendar, and Drive.
sidebar:
  order: 16
---

Connecting Google Workspace gives sessions two things. The organization connection reads the
directory: people and their managers, groups and members, organizational units, domains, and
the audit reports. A member's own connection reaches their Gmail, Calendar, Drive, Docs,
Sheets, Slides, and contacts as them.

The organization connection is a Google service account with domain-wide delegation for the
directory read scopes only. It acts as one account you name, and Google refuses it a token for
anything outside the delegated scopes, so it can never read a mailbox or a drive. engrams signs
a short-lived token request with the sealed key and obtains a new token before each one
expires; there is no consent screen and nothing to renew by hand.

## Create the service account

1. In the Google Cloud console, create a project (or pick one) and open IAM & Admin →
   Service accounts. Create a service account; it needs no project roles.
2. Open the service account, go to Keys, and add a JSON key. Keep the downloaded file; you
   paste it into engrams.
3. Note the service account's numeric client ID, shown on its details page.

## Delegate the directory scopes

In the Google Admin console, open Security → Access and data control → API controls →
Manage domain-wide delegation, and add the client ID with exactly these scopes:

```
https://www.googleapis.com/auth/admin.directory.user.readonly,
https://www.googleapis.com/auth/admin.directory.group.readonly,
https://www.googleapis.com/auth/admin.directory.group.member.readonly,
https://www.googleapis.com/auth/admin.directory.orgunit.readonly,
https://www.googleapis.com/auth/admin.directory.domain.readonly,
https://www.googleapis.com/auth/admin.reports.audit.readonly
```

Delegate nothing else. The account the connection acts as must be able to read the
directory; a user with the Groups Reader and User Management Reader admin roles is enough.
Google can take some minutes to apply a new delegation.

## Create the OAuth client

Members connect their own account through an OAuth client of your organization.

1. In the same Google Cloud project, open APIs & Services → OAuth consent screen and choose
   the Internal user type. An internal app needs no Google verification.
2. Under Credentials, create an OAuth client ID of the Web application type and add this
   redirect URI:

   ```
   <origin>/api/v1/integrations/google_workspace/oauth/callback
   ```

3. Copy the client ID and client secret.
4. Enable the APIs the sessions use under APIs & Services → Library: Admin SDK, Gmail,
   Google Calendar, Google Drive, Google Docs, Google Sheets, Google Slides, and People.

## Connect it

Open Settings → Integrations → Google Workspace → Connect. Paste the service account's JSON
key, the email of the account it acts as, and the OAuth client ID and secret, then select Add
Google Workspace. engrams seals the values as the org secrets
`google_workspace.service_account_key`, `google_workspace.client_id`, and
`google_workspace.client_secret`, obtains the organization token, seals it, and marks the
integration connected. Reconnect from the integration page obtains a fresh token with the
same account.

Each member who needs their own Gmail, Calendar, or Drive connects under Settings →
Credentials → Google Workspace, which opens Google's consent screen for the scopes the
connector asks for. A profile chooses per integration whether a human session uses the
member's connection or the organization's.

The tokens never enter a sandbox. The egress proxy adds the right one to each request as a
bearer token and removes any token the agent supplies.

## What sessions get

A profile grants powers by API area. The organization connection serves `directory:read`
and `reports:read`. The personal areas are `gmail:read`, `gmail:write`, `calendar:read`,
`calendar:write`, `drive:read`, `drive:write`, `docs:read`, `docs:write`, `sheets:read`,
`sheets:write`, `slides:read`, `slides:write`, and `contacts:read`.

The `gws` command, Google's Workspace CLI, is on the PATH in every session whose profile
grants at least one Google Workspace power. Its commands follow Google's API reference for
every service, so `gws admin users list`, `gws drive files export`, and `gws gmail users
messages list` all take the same `--params` JSON that the API documents. Run `gws --help`, or
`engrams-integrations` for every connected tool.

```bash
gws admin users get --params '{"userKey":"someone@example.com","projection":"full"}'
gws admin members list --params '{"groupKey":"team@example.com"}'
gws drive files export --params '{"fileId":"<id>","mimeType":"text/plain"}'
gws calendar events list --params '{"calendarId":"primary","timeMin":"2026-11-01T00:00:00Z"}'
```

A user's manager is the `relations` entry of type `manager` on the user record. It is filled
in only when your HR system writes it to Google; Rippling's Google Workspace integration has
a Manager attribute mapping that is off by default.

Google sends no plain webhooks, so an automation that reacts to Workspace data runs on a
schedule and reads with the commands above.
