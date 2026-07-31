# ADR 0107: Portable Google Cloud access through Workload Identity Federation

Status: 2026-07-31 — **Proposed.**

## Context

Sessions need to call Google Cloud APIs without receiving the deployment's node
identity or a long-lived service-account key. The same configuration must work
when engrams runs on GCP, AWS, Azure, or on-premises.

ADR 0056 makes the host egress interceptor the integration gate. ADR 0057 makes
the profile the session-policy source. Those decisions keep provider credentials
outside the guest, but the current provider registry has one credential instance
per provider and profiles carry flat capability strings. That shape cannot safely
select one of several Google service accounts. It can also lose the association
between an identity, an operation, and its resource limit when grants are merged.

The current orchestrator create path also writes `task_session` after the
coordinator has booted the session. A WIF broker must authorize the first token
exchange during boot. It cannot depend on a row that does not exist yet.

## Decision

### Named connections and structured grants

The orchestrator owns named `IntegrationConnection` rows. A connection names one
provider and one configured identity. A Google Cloud connection contains only:

- the full workload identity provider resource;
- one target service-account email;
- an exact endpoint ceiling;
- display metadata and an enabled flag.

Profiles bind structured grants:

```text
ProfileIntegrationGrant {
  connection_id
  operation
  resource_constraints
}
```

The compiler keeps this tuple intact while it produces the immutable session
policy. Skills and per-session skill selection never add or change grants.

Existing provider credentials become deterministic default connections. Existing
profile capabilities migrate to structured grants on those connections. The
coordinator can continue to persist its provider capability projection for
provider-specific token scoping, but it is no longer the profile API.

### Restricted profiles

A profile can be organization-readable or restricted. A restricted profile has
explicit principal launch grants. Administrators can always launch it. A profile
that binds a Google Cloud connection must be restricted. Profile list, get, and
task create all enforce the same rule on the server.

### WIF-only authentication

Engrams is an OIDC identity provider for Google WIF. It publishes public discovery
and JWKS documents under its configured public HTTPS URL. The signing key is
KEK-sealed at rest. Rotation publishes the old and new public keys during an
overlap window.

The OIDC token has an immutable session UUID as `sub`. It also carries immutable
deployment, connection, user, and profile-snapshot identifiers. Its `aud` is the
exact Google workload identity provider resource. The broker exchanges it at
Google STS and then calls IAM Credentials `generateAccessToken` for the
connection's fixed service account.

No service-account key format is accepted. The deployment's ambient identity is
not consulted.

### Durable authorization before boot

The trusted orchestrator allocates the session UUID. It stores the task/session
row and effective connection grants before it sends `CreateSession` to the
coordinator. The coordinator accepts that requested UUID from its authenticated
orchestrator client. On a create failure, the orchestrator removes the provisional
rows. It registers event listeners only after the coordinator accepts the create.

The coordinator calls an authenticated orchestrator-internal WIF broker endpoint
when it needs a Google token. The broker resolves the immutable session snapshot,
checks that the connection is still enabled, issues an OIDC token, performs the
two Google exchanges, and returns the short-lived Google access token to the
coordinator. The token then follows ADR 0056's host-only inject path. It never
enters the guest.

Connection disablement is a live kill switch. Every refresh checks the current
connection row. Profile edits affect only new sessions.

### Guest compatibility and request policy

The guest sees a session-local GCE metadata endpoint that returns an opaque
placeholder access token. The real metadata service remains unreachable. The
egress proxy overwrites `Authorization` with the brokered token only for a request
that matches the connection's compiled endpoint and operation policy.

Google IAM is the resource ceiling. Different privilege sets use different
service accounts and connections. The proxy adds defense in depth. It blocks
Google STS, OAuth, service-account key creation, `generateAccessToken`,
`generateIdToken`, `signJwt`, and other credential-producing operations from the
guest even under a broad API grant.

Upstream TLS verification and HTTP/2 support are prerequisites for enabling the
Google connector. Tokens, authorization headers, and token-exchange bodies are
never logged.

## Consequences

- A deployment needs a public HTTPS issuer and JWKS endpoint to use Google Cloud
  connections.
- The orchestrator becomes part of Google token refresh. The coordinator remains
  the host-agent's refresh endpoint, so the guest and host network topology do not
  change.
- A Google Cloud connection can be moved between deployment clouds without a
  credential migration.
- WIF audit subjects can be correlated with engrams session audit records.
- Kubernetes RBAC, database credentials, product API keys, and Google IAM remain
  separate authorization systems.

## Phasing

1. Add named connections, structured grants, restricted profile launch grants,
   and the orchestrator-selected session UUID.
2. Add the OIDC issuer, signing-key rotation, WIF broker, and Google connector.
3. Add metadata compatibility, verified upstream TLS, HTTP/2, and credential API
   denials in the egress proxy.
4. Add the admin UI, Google setup generator, image tools, and skill updates.
5. Accept this ADR after focused security tests and real GCP staging smokes pass.
