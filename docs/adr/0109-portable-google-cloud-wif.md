# ADR 0109: Portable Google Cloud access through Workload Identity Federation

Status: 2026-07-31 — Proposed. 2026-08-02 — **Accepted.**

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
policy. Session launch also stores a non-secret snapshot of each selected
connection. Later connection edits do not change that session snapshot.

Existing provider credentials receive ordinary default connections with opaque
IDs. Existing profile capabilities migrate to structured grants on those
connections. Migration state is not encoded in connection IDs or runtime
branches. The coordinator can continue to persist its provider capability
projection for provider-specific token scoping, but it is no longer the profile
API. Google
Cloud connections follow the same organization-wide profile authorization model
as existing integrations. User-scoped integration identities are a separate
future concern.

### WIF-only authentication

Engrams is an OIDC identity provider for Google WIF. It publishes public discovery
and JWKS documents under its configured public HTTPS URL. The current product is
single-organization per deployment, so the issuer and signing-key set are
deployment-specific. The signing key is KEK-sealed at rest. Rotation publishes
the old and new public keys during an overlap window.

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

ADR 0056's mint path uses a typed connection source. GitHub App credentials and
Google Cloud both carry the opaque connection ID selected by the profile. The
provider is separate routing metadata and is never inferred from that ID. Both
return the same `ScopedCredential` shape and use the same boot, refresh,
header-rendering, and host-proxy injection code. There is no Google branch in
the host agent or guest agent.

The coordinator calls one authenticated orchestrator-internal connection
credential endpoint when a named source needs a credential. The broker resolves
only the immutable session snapshot. For Google Cloud, it issues an OIDC token,
performs the two Google exchanges, and returns a short-lived bearer credential to
the coordinator. The credential then follows ADR 0056's host-only inject path. It
never enters the guest.

Disabling, editing, or deleting a connection prevents future sessions from using
it. An active session keeps its launch-time snapshot and can refresh credentials
from that snapshot until the session ends. Revoking the Google-side WIF or IAM
binding can still stop later exchanges outside Engrams.

### Guest compatibility and request policy

The host egress process serves a session-local GCE metadata-compatible endpoint
that returns an opaque placeholder access token. Firecracker and VZ steer only
the metadata address to this listener. The real metadata service remains
unreachable. The Process backend has no network boundary, so it rejects sessions
that request Google ADC. The egress proxy overwrites `Authorization` with the
brokered token only for a request that matches the connection's compiled endpoint
and operation policy. It also overwrites the HTTP `Host` or HTTP/2 authority with
the validated TLS server name, so an allowed frontend cannot route a request to
another virtual host. `engram-agentd` has no Google integration code.

Google IAM is the resource ceiling. Different privilege sets use different
service accounts and connections. The proxy adds defense in depth. It blocks
Google STS, OAuth, service-account key creation, `generateAccessToken`,
`generateIdToken`, `signJwt`, and other credential-producing operations from the
guest even under a broad API grant.

Upstream TLS verification and HTTP/2 support are prerequisites for enabling the
Google connector. Tokens, authorization headers, and token-exchange bodies are
never logged. If an upstream response echoes a brokered credential, the proxy
redacts it before any response byte reaches the guest.

The credential broker logs identity and mint outcomes. The egress proxy logs
the matching session, connection source, target, request shape, and policy
outcome. It removes query strings and never logs request bodies or credential
headers.

## Consequences

- A deployment needs a public HTTPS issuer and JWKS endpoint to use Google Cloud
  connections.
- Google-enabled sessions require a Firecracker or VZ host with the egress proxy.
  The insecure Process backend fails closed.
- The orchestrator becomes part of named-connection credential refresh. The
  coordinator remains the host-agent's generic refresh endpoint.
- A Google Cloud connection can be moved between deployment clouds without a
  credential migration.
- WIF audit subjects can be correlated with engrams session audit records.
- Kubernetes RBAC, database credentials, product API keys, and Google IAM remain
  separate authorization systems.
- *(Amended 2026-08-03, ADR 0106 addendum:)* connection rows additionally serve
  as the OAuth credential **subjects** for oauth-facet connectors
  (`oauth_credentials.subject_id` = the connection id). The rows themselves
  still never carry a credential.

## Phasing

1. Add named connections, structured grants, and the orchestrator-selected
   session UUID.
2. Add the OIDC issuer, signing-key rotation, WIF broker, and Google connector.
3. Add metadata compatibility, verified upstream TLS, HTTP/2, and credential API
   denials in the egress proxy.
4. Add the admin UI, Google setup generator, and authenticated `gcloud` bundle.
5. Accept this ADR after focused security tests and real GCP staging smokes pass.

## Divergences, closeout, and lessons

A deep quality review after the first implementation landed (#930-#964) found
the decision sound and the implementation short of it in specific, verifiable
ways. What follows is what changed and why, so a reader of this ADR sees the
shipped design rather than the proposed one.

### The egress gate

**The HTTP/1 header gate was bounded, the header block was not.** The adapter
buffered a request prefix, gated on it, and relayed the tail. A guest that
padded its headers past the buffer put its own `Host` or `Authorization` beyond
the gate's view. The block is now buffered whole or the request is refused, and
every line shape the CRLF walkers and the upstream would read differently — a
bare LF, an obsolete fold, whitespace before the colon, `Content-Length` beside
`Transfer-Encoding` — is rejected.

**`Content-Length` went stale after substitution.** A real secret is longer than
its placeholder. HTTP/2 drops the header because it has a structural length;
HTTP/1 has none, so a broker-mode secret in a request body truncated or hung the
request. The declared length is now rewritten from what is actually sent.

**Response redaction was not chunk-aware.** A chunked upstream can split a
credential across a chunk boundary, where the wire bytes carry `\r\n<size>\r\n`
between the halves; the raw scan found nothing and the credential reached the
guest whole. The scan now runs over the DECODED stream and writes matches back
in place. Redaction is length-preserving, so chunk sizes stay exact and no
re-framing is needed. This de-chunks rather than rejecting chunked responses,
because Google frontends send them.

**The credential denylist ran in the wrong place and was written twice.** It
lived inside the intercept path, so a broad `*.googleapis.com` network allow
beside a narrow injection spliced STS straight through with no gate at all. It
matched only REST shapes, so the gRPC form of service-account key creation was
allowed. It did not know the mutual-TLS twins (`sts.mtls.googleapis.com`), which
against an exact-host list is a complete bypass. And the orchestrator kept its
own copy, which already knew a different set of hosts.

The denylist is now ONE checked-in table
(`crates/engram-egress-proxy/policy/google-credential-denylist.json`) that the
proxy `include_str!`s and the orchestrator imports. A denied HOST is refused at
admission, in `SessionState::decide()`, before any TLS — which is what covers a
bypass connection. A denied OPERATION needs the request line, so a host carrying
one is never spliced: admission upgrades it to an intercept with an empty
policy. `X-HTTP-Method-Override` is stripped on both protocols, because Google
honours it and it otherwise defeats every method-gated rule.

**One gate, not two.** The HTTP/1 and HTTP/2 adapters each carried a hand-copied
version of the policy gate. They had already drifted: the placeholder-leak
detector could not fire at all (`decide()` narrows secrets to the host-matching
ones, and the scan then asked which of THOSE the host disallows — always none),
and the e2e fixture built its own list, so it tested the same empty set and
passed. Both adapters now run one `evaluate_stream`, and `decide()` supplies the
foreign placeholders beside the narrowing that hid them.

**Reachability and resolution must answer the same question.** Making the mint
fail closed meant no longer adding a Google host to `network_allow` —
reachability rides the injection, so a failed mint leaves nothing reachable. That
immediately exposed a latent gap: the DNS gate consulted only `network_allow`
and secrets, never injections. A host the proxy was willing to intercept was one
the guest could not look up.

### Authorization and the issuer

- A "read" grant allowed writes. Curated operations emitted one entry with
  `["GET","POST"]` shared across the REST and gRPC paths, which the proxy matches
  independently — so `timeSeries.create` was permitted under a `timeSeries.list`
  grant, and the test suite pinned it. Methods are now bound per path.
- Broker authorization is bounded by session LIFETIME. It checked only
  `task_session` rows, which outlive the session, so it minted for ended sessions
  forever. It also used the shared control-plane bearer; it has its own now.
- Signing-key rotation was implemented and had no caller. It runs on a schedule
  with an admin trigger, on the ADR-0098 `spawn`/`run_once` split.
- The claims were wrong. `engrams_organization` carried the issuer URL, already
  present in `issuer_uri`; `engrams_profile_snapshot` was `${profileId}:${sessionId}`,
  not an immutable snapshot id. They now carry a deployment id and a content hash
  of the compiled snapshot, persisted on `task_session`. **This is a clean
  break**: an existing WIF pool whose attribute condition pins the issuer URL
  must re-apply the regenerated setup.
- A successful brokered call left no audit trace — only denials and mint
  outcomes were logged, which this ADR's own closeout note flagged as owed. One
  structured event per permitted request now records session, connection source,
  target, method and query-free path.

### The provider seam

This ADR says the design has "no runtime branches on the provider". The first
implementation did not hold to that: `provider === "gcp"` appeared at nine call
sites plus a `startsWith("gcp:")` in profile save. Each was a place a second
provider would have to be threaded through by hand.

Three seams close it. `ConnectionProvider` in the orchestrator carries config
validation, the operation catalog, grant validation, policy compilation,
minting, the setup document, and the guest environment a credential needs to be
findable; the registry hands each provider only its OWN grants, so the Google
functions dropped their internal provider filters. `MetadataFlavor` replaces
`google_adc: bool` on the wire, because that boolean conflated "does this
session need a metadata endpoint?" with "is it Google's?"; the proxy resolves a
flavor to a `MetadataService` through a wildcard-free `match`, so a second cloud
is a compile error rather than a silently unserved session. And the integration
catalog serves named-connection providers, so the web renders from the same
table the orchestrator compiles policy from instead of a hand-maintained copy.

### Lessons

**A duplicated table is a security bug waiting for time to pass.** The denylist,
the curated operation list, and the endpoint gate each existed twice. In every
case the copies had already diverged, and in every case the divergence favoured
the permissive side.

**A test that cannot fail is worse than no test.** The placeholder-leak e2e built
its own inputs and asserted on a set that production makes empty. It passed for
months while the detector was unreachable. Its fixture now drives
`SessionState::decide()`, and it fails against the old code.

**Read the flake.** Four e2e tests failed about once in twenty runs behind a
growing list of "tolerated" errors. Instrumenting each leg found three unrelated
causes — one of them a real proxy behaviour (a normal upstream
`Connection: close` reported as a failure of the whole intercept), the other two
fixtures that only worked when data happened to arrive in one segment. The
tolerated-error lists are deleted.

**Column drops are two-phase in principle.** Migration 0044 dropped
`profile.capabilities` inside the same transaction that read it. That is safe
only pre-users, and it is already applied and therefore immutable. A later drop
should stop writing in one release and drop in the next.

### Closeout

Phases 1-4 shipped in #930-#964. The remediation above shipped as four reviewed
changes: the egress proxy hardening, the orchestrator policy and issuer work,
the web surfaces, and the provider seam.

Production smokes covered GKE, Logging, Monitoring and Trace over REST. The
HTTP/2 leg — the one a real Google gRPC call takes — had never been exercised
end to end; it now has two tests through `intercept::run` with ALPN on both
legs, and a live gRPC smoke is the remaining verification before the next
deploy.
