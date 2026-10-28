# ADR 0125: Google Workspace connector and the service-account grant

Status: Proposed (2026-10-28)

Terms used in this document:

- **Directory plane**: organization data in the Admin SDK: users and their
  managers, groups and members, organizational units, audit reports.
- **Personal plane**: a person's own data behind the Gmail, Calendar, Drive,
  Docs, Sheets, Slides, and People APIs.
- **Service account**: a Google Cloud identity with an RSA key pair. It has no
  mailbox and no seat.
- **Domain-wide delegation**: the Workspace setting that lets a service
  account obtain tokens that act as a user of the domain, for an explicit
  list of scopes. The token request names the user in the JWT `sub` claim.
- **Principal**: the one domain user the platform acts as when it runs the
  delegated grant.
- **JWT-bearer grant**: RFC 7523. The client signs a JWT and exchanges it at
  the token endpoint for an access token; no browser, no refresh token.

## Summary, in plain English

Google Workspace is two products for an agent. The directory plane is
organization data and the platform reads it as the organization. The
personal plane is a person's data and a session reads it as that person,
with that person's consent. One connector serves both, with two
credentials: a service account for the organization and the existing
user-scoped OAuth facet (ADR 0115) for people.

The service account needs a third grant family in the coordinator, next to
the authorization-code and client-credentials grants: the JWT-bearer grant.
It is the same shape of change as ADR-less client credentials in #1702: a
spec from the orchestrator, one org secret resolved coordinator-side, a
sealed token with a refresh spec, and the existing refresh sweep.

The service account can impersonate only the directory, never a mailbox or
a drive. Two controls make that true: the admin delegates the directory
scopes and no others, so Google refuses any other token for that key; and
the platform signs every token request as one fixed principal, with the key
sealed in the org secret store and never present in a sandbox.

The CLI is Google's own `gws`, bundled with a placeholder token that the
egress proxy replaces. No events in this version: Google has no plain
webhooks, and a schedule covers the known use cases.

## Context

A compliance automation has to name an employee's manager. Rippling holds
that relation but its API is a paid add-on; Google Workspace holds it once
Rippling's attribute sync writes the manager (the sync is a Rippling setting,
off by default). The platform has no Google connector.

The connector model (ADR 0058, ADR 0106 addendum, ADR 0115) supports an
org-scoped OAuth connection obtained through a consent screen or, since
#1702, through client credentials; a user-scoped OAuth connection over the
same facet; static tokens; and mint engines. Google's organization identity
fits none of these:

- A consent screen yields a token for the human who clicked. The Admin SDK
  answers only to an admin, so the organization token would be one
  administrator's personal token, and it dies with their password change.
- Client credentials do not exist at Google. The equivalent is a service
  account with an RSA key: the client signs a JWT (`iss` the service
  account's email, `scope` the space-joined scopes, `aud`
  `https://oauth2.googleapis.com/token`, optional `sub` the user to act as,
  `exp` at most one hour after `iat`, RS256) and posts it with
  `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer`. The response is
  an access token that lives one hour. There is no refresh token.

The personal plane is a direct fit for ADR 0115: the user authorizes a BYO
OAuth client of their own organization through the authorization-code flow.
An OAuth client created inside the customer's Workspace is an internal app,
so Google's verification review for restricted Gmail and Drive scopes does
not apply. An admin may still have to allow the app under API controls.

## Decisions

### D1. One connector, two credentials

The connector is `google_workspace`. Its org credential is a service-account
token obtained by the JWT-bearer grant. Its user credential is the ADR 0115
OAuth flow over the same facet. Profiles choose per integration which one a
human session uses, as they do for Slack and Linear today.

Why one connector: the admin registers one OAuth client and delegates one
service account; powers by area give the same granularity that separate
connectors would, and the egress proxy enforces them per path regardless of
which token is injected.

### D2. The JWT-bearer grant family

The OAuth facet's `grant` gains the value `service_account`:

```jsonc
"oauth": {
  "grant": "service_account",
  "tokenUrl": "https://oauth2.googleapis.com/token",
  "serviceAccountKeyRef": "google_workspace.service_account_key",
  "scopes": [ /* the delegated scopes: the directory plane only */ ],
  "scopeDelimiter": " ",
  // The user flow only; the org grant never reads these.
  "authorizeUrl": "https://accounts.google.com/o/oauth2/v2/auth",
  "clientIdRef": "google_workspace.client_id",
  "clientSecretRef": "google_workspace.client_secret",
  "metadata": { "probe": { /* customer id and domain */ } }
}
```

Parse rules in the orchestrator registry:

- `serviceAccountKeyRef` is required for, and only for, `service_account`.
- A service-account connector always declares `userCredential.oauth`; the
  facet's `authorizeUrl`, `acquisitionHosts`, `clientIdRef`, and
  `clientSecretRef` serve that personal flow and nothing else. Keeping the
  client refs required leaves every consumer of the facet unchanged.
- `tokenRequestBody`, `extraAuthorizeParams`, and `pkce` do not apply: the
  org grant has no browser leg and posts a form.

The coordinator gains `GrantServiceAccount` next to `GrantClientCredentials`.
The spec carries the token URL, the scopes, the key ref, the principal, and
the metadata spec. The grant resolves the key (a Google service-account JSON
file: `client_email`, `private_key`, `token_uri`), checks that `token_uri`
equals the facet's token URL, signs the JWT with `jsonwebtoken` (RS256, the
crate the GitHub App path already uses), posts the assertion, and seals a
`ConnectorOAuthBundle` of the new kind `Oauth2JwtBearer`. The sealed refresh
spec records the key ref, the principal, the scope, and the token URL, so the
refresh sweep re-runs the grant without the connector catalog, as it does
for client credentials. The fields default on deserialization; bundles sealed
before this change are unchanged.

The connector lists no metadata mapping: Google's token response names no
account, and the directory probe would fail under a member's token. The
coordinator fills `display_name` with the principal for the org grant.

The principal is an input of the grant request, entered once in the connect
sheet. Reconnect from the integration page re-runs the grant; when the
request names no principal the coordinator takes it from the sealed spec.
The integration page shows the principal as the connection's display name.

Rejected: a dedicated bot Google user signed in through the consent screen.
It needs a super-admin seat whose refresh token any password or policy
change revokes. Rejected: a connection setting for the principal. The
`settings` facet exists for hosts that join the egress allow-list; the
principal joins nothing, and a sealed-spec field is enough for this version.

### D3. Impersonation stays inside the directory

The facet's org `scopes` are the directory read scopes and the reports read
scope, and nothing else:

```
https://www.googleapis.com/auth/admin.directory.user.readonly
https://www.googleapis.com/auth/admin.directory.group.readonly
https://www.googleapis.com/auth/admin.directory.group.member.readonly
https://www.googleapis.com/auth/admin.directory.orgunit.readonly
https://www.googleapis.com/auth/admin.directory.domain.readonly
https://www.googleapis.com/auth/admin.reports.audit.readonly
```

The guide tells the admin to delegate exactly this list in the Admin console
(Security, API controls, domain-wide delegation). Google refuses a token for
any scope outside the delegated list, so even the raw key cannot reach a
mailbox or a drive. The key is an org secret; the sandbox sees only the
proxy-injected token for one fixed principal.

Directory writes (create a user, add a member) are not in this version. They
would widen the delegated list and the impersonation surface, and no use
case asks for them. Adding them later is a scope list change and a set of
write powers; the admin re-delegates and reconnects.

### D4. The personal plane through ADR 0115

`userCredential.oauth` overrides the scopes with the personal set and adds
Google's `access_type=offline` and `prompt=consent` authorize parameters so
the exchange returns a refresh token:

```
openid email
https://www.googleapis.com/auth/gmail.readonly
https://www.googleapis.com/auth/gmail.send
https://www.googleapis.com/auth/gmail.modify
https://www.googleapis.com/auth/calendar
https://www.googleapis.com/auth/drive
https://www.googleapis.com/auth/documents
https://www.googleapis.com/auth/spreadsheets
https://www.googleapis.com/auth/presentations
https://www.googleapis.com/auth/contacts.readonly
```

The user token is the ordinary authorization-code bundle; the refresh sweep
already handles it. The app is internal to the customer's organization, so a
single consent for the full personal set is the expected shape; a narrower
set is a connector JSON edit if a deployment wants one.

### D5. Powers by API area

| Power | Hosts and paths |
|---|---|
| `directory:read` | `admin.googleapis.com` `/admin/directory/v1/users*`, `/groups*`, `/customer/*/orgunits*`, `/customers/*`, `/admin/directory/v1/customer/*/domains*` (GET) |
| `reports:read` | `admin.googleapis.com` `/admin/reports/v1/*` (GET) |
| `gmail:read` | `gmail.googleapis.com` `/gmail/v1/users/*` (GET) |
| `gmail:write` | `gmail.googleapis.com` `/gmail/v1/users/*` (POST, PUT, PATCH, DELETE) |
| `calendar:read` / `calendar:write` | `www.googleapis.com` `/calendar/v3/*` |
| `drive:read` / `drive:write` | `www.googleapis.com` `/drive/v3/*`, `/upload/drive/v3/*` |
| `docs:read` / `docs:write` | `docs.googleapis.com` `/v1/documents*` |
| `sheets:read` / `sheets:write` | `sheets.googleapis.com` `/v4/spreadsheets*` |
| `slides:read` / `slides:write` | `slides.googleapis.com` `/v1/presentations*` |
| `contacts:read` | `people.googleapis.com` `/v1/people*`, `/v1/otherContacts*` |

Read powers match GET; write powers match the mutating methods. A session
under the org token that asks for a personal power reaches Google with a
token that has no such scope and gets a 403 from Google; the proxy's job is
the allow-list, the token's scopes are the second fence. The test probe is
`GET admin.googleapis.com/admin/directory/v1/customers/my_customer`.

### D6. The CLI is Google's `gws`

The `integrations-cli` bundle fetches the pinned `google-workspace-cli`
release for the build architecture (Linux gnu builds exist for amd64 and
arm64), next to `gh` and `pup`. The connector's `cli` facet sets
`GOOGLE_WORKSPACE_CLI_TOKEN` to the managed placeholder; the proxy replaces
the bearer header host-side. `gws` builds its commands from Google's
Discovery documents, so one binary covers every service above with one
command shape (`gws <service> <resource> <method> --params '<json>'`).

Risks and their handling: the project is pre-1.0 and expects breaking
changes, so the bundle pins the version and the archive checksum, and the
`cli.doc` text names the pinned command shape. The binary must run with
only the token variable set and no config directory; the connector phase
verifies this in a session before the PR lands. If `gws` cannot run that
way, the fallback is a generic `google api <service> <path>` wrapper of the
Vanta shape, at a day's cost.

### D7. No events

Google pushes Drive and Calendar changes through watch channels that expire
and need renewal, and Gmail only through Cloud Pub/Sub in the customer's
project. No use case needs them now. Automations poll on a schedule.

## Consequences

- A new grant family in the coordinator, the protocol, the orchestrator, and
  the web sheet. Client credentials made the seams; this is the second user
  of them.
- The connect sheet has a new mode: a service-account key (a JSON file
  pasted as a secret), a principal, and, for the user flow, the OAuth
  client id and secret.
- A fetched third-party binary in the bundle with the same pin discipline
  as `pup`.
- The admin does three things once in Google: create the service account
  and key, delegate the directory scopes to its client id, and create an
  internal OAuth client with the platform's callback URL.

## Phases

1. **Platform**: the ADR, `GrantServiceAccount` and `Oauth2JwtBearer` in
   the coordinator with tests (JSON key parse, JWT claims, principal
   fallback on re-grant, refresh re-run), the registry facet rules, the
   orchestrator grant route, the web sheet mode.
2. **Connector**: `google_workspace.json`, the bundled `gws`, the skill and
   guide text, the logo, and a live check against a real Workspace.
3. **Acceptance**: flip this ADR to Accepted with the commit chain.

## Verification

- Coordinator: `cargo nextest run -p engram-coordinator` with a fake token
  endpoint that checks the assertion's claims and signature.
- Orchestrator: facet parse tests for every rule in D2; the grant route for
  `service_account`; the shipped connector's powers and probe.
- Web: the sheet in service-account mode seals the key, posts the grant with
  the principal, and closes.
- Live: connect a real Workspace, run `gws admin users list` and the
  directory probe in a session, confirm a manager email appears on a user,
  and confirm a personal-plane call fails under the org token.
