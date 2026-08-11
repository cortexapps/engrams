# ADR 0115 — user-scoped integration credentials

Status: 2026-08-11 — Proposed. 2026-08-11 — **Accepted.** Implementation
chain (all merged): #1176 (coordinator substrate), #1177 (wire v28 + session
boot), #1180 (orchestrator compile + gate), #1181 (/me routes + shared
callback + Slack surfacing), #1182 (web), #1183 (connector adoption: linear,
slack, sentry, github).

## Context

Every connector credential today is org-scoped. Static-inject connectors
resolve a `secretRef` from the coordinator's `org_secrets` store (ADR 0057).
The `github` connector mints installation tokens from the org GitHub App
(ADR 0056). OAuth-facet connectors (`linear`, `slack`) hold one org-wide
connection whose sealed credential lives in the coordinator's
`oauth_credentials` store under subject `(connector, <connection_id>)`
(ADR 0106 addendum). Every session that a profile launches therefore acts as
one shared org identity, regardless of who launched it.

ADR 0109 deferred this problem: "user-scoped integration identities are a
separate future concern." This ADR resolves that deferral.

The harness tier already has the shape we need. A harness descriptor declares
`auth.user_env` or `auth.user_oauth` for human principals and `auth.org_env`
for programmatic principals (ADR 0063, ADR 0106). One switch decides:
`isHuman = !programmatic`, computed from the principal, never from the task
type or surface. Missing human credentials block launch with a
`FailedPrecondition` error, and the web client mirrors the rule with a
pre-launch banner that links to Settings → Credentials. This ADR extends that
exact contract to connectors.

## Decision

### Two credential axes per connector, declared in one JSON file

A connector keeps its required org-scoped credential: a `secretRef` inject, a
connector-subject OAuth connection, or a mint engine. A connector MAY also
declare user-scoped support with a new optional `userCredential` facet:

```jsonc
"userCredential": {
  "oauth": true,                             // user-subject flow over the top-level oauth facet
  "token": { "hint": "Create a PAT at ..." },  // personal-access-token mode
  "inject": { "header": "Authorization", "template": "Bearer {}" }  // mint connectors only
}
```

Parse invariants:

- At least one of `oauth` / `token` must be present.
- An inject-source connector must have exactly one inject header. The user
  value renders through that same header and template. Multi-header
  connectors (`datadog`) cannot declare user support in this version.
- A mint connector (`github`) may declare `token` mode only, and must supply
  `userCredential.inject`, because the mint engine renders the org header.
  User-to-server OAuth for GitHub is deferred.
- `oauth: true` requires the top-level `oauth` facet. The existing facet
  cross-checks do not change.

GitHub's user mode is PAT-only by design: each user selects the permissions
their fine-grained PAT carries. User-scoped GitHub covers the connector
egress plane (`api.github.com` operations). Git forge operations keep the
broker/forge-token path (ADR 0023).

### Storage: one sealed store, a new subject kind

User connector credentials — OAuth bundles and PATs — live in the
coordinator's `oauth_credentials` store under a new subject kind
`user_connector`, keyed `(user_connector, <orchestrator user id>, <provider>)`.
One credential slot exists per user and provider: an OAuth connect replaces a
stored PAT, and a PAT write replaces an OAuth connection.

A PAT seals as a `ConnectorOAuthBundle` with a new kind `static_token`, no
expiry, and no refresh spec. The refresh sweep can never select it, and it is
never marked broken. A new `PutCredential` RPC writes it with
compare-and-swap semantics. A new `LookupRedirectFlow` RPC returns the
non-secret subject of a flow so one registered OAuth callback URL serves both
org-subject and user-subject completions.

We do not reuse the harness `user` subject kind. The refresh sweep selects
rows by kind and parses connector bundles; harness bundles are opaque and
refresh through the session control channel. A distinct kind also removes
provider-name collisions between the two planes, and makes one
`ListCredentials` call the exact launch-gate query.

We do not store user PATs in the orchestrator's `user_session_secrets` table.
Session policies persist in three places (task `launch_policy`, the
coordinator policy row, node-local `.egress` files). Policies must stay
value-free; the sealed store keeps them so.

### Wire: the compiled policy stamps the subject

`CredentialMintSource` gains an appended variant
`OauthUser { user_id, connection_id, provider }` (wire version 27 → 28). The
orchestrator stamps the launching user's id at compile time. The coordinator
stays principal-agnostic: session create carries no user id, and every
re-resolution path (resume, reattach, host `.egress` refresh) re-reads the
persisted policy. `connection_id` rides the wire for future per-connection
user credentials; this version ignores it for key derivation.

The coordinator does not enforce the profile toggle. The orchestrator is the
enforcement point at create time. A credential that disappears after create
(disconnect, revocation) soft-fails at boot: the inject entry is skipped and
provider requests return 401, the same posture as an org mint failure.

For user-sourced tokens without an expiry, the parked proxy-refresh horizon
clamps to 24 hours (org static secrets keep the existing behavior). A
disconnect or PAT replacement therefore reaches live sessions within a day.

### Profiles opt in per integration

`ProfileIntegrationGrant` gains `credentialScope: "org" | "user"` (absent
means org). The field rides the existing grant array through
`task.launch_policy`, the `task_session` snapshot, and the sub-session
synthetic profile, so no new snapshot column exists and the audit trail
records the acting scope for free.

Profile-save validation enforces:

- user scope only when the connector declares `userCredential`;
- one scope across all grants of one connection;
- no user scope when two granted connections share one provider. The v1
  subject is per user and provider; two same-provider connections would
  resolve one user token into both identities.

### The launch gate

In `compileSessionCreateInput`, inside the existing `isHuman` branch:

- One `ListCredentials(user_connector, <owner user id>)` call per compile
  serves every user-scoped integration and the harness OAuth check.
- Every user-scoped provider must have status `connected`. A missing or
  broken credential raises one `FailedPrecondition` that names all missing
  providers and points to Settings → Credentials.
- Programmatic sessions and capability-override sessions compile org
  credentials, always. Sub-session spawn re-checks presence only when the
  parent's frozen policy stamped a user subject.

The web client mirrors the gate: the start screen computes missing personal
credentials from the selected profile's grants and blocks launch with the
same banner family as harness credentials. Settings → Credentials gains
integration cards (OAuth connect, PAT entry, disconnect). Slack-triggered
sessions surface the `FailedPrecondition` message in-thread with a
credentials link instead of the generic failure text.

### Amendment (2026-08-11): warn and disable, never block

A missing or unhealthy personal credential no longer blocks a human launch.
The compile DROPS the unsatisfied user-scoped grants instead: the
integration contributes no capability, no tool or CLI surface, no egress
entry, no opened host, and no snapshot grant for that session. There is
still no org fallback — the integration is off, not downgraded. The start
screen keeps the same warning box (naming the disabled integrations, with
the Settings → Credentials link) but leaves the launch enabled. The harness
credential gate is unchanged: a session without it boots unauthenticated,
so it still blocks.

## Consequences

- ADR 0057's single-tenant model stays. User-scoped means per-user inside
  the one org, not multi-tenant.
- The refresh sweep runs over both `connector` and `user_connector` kinds,
  so idle user OAuth credentials stay fresh and broken ones show truthfully.
- Personal tokens now share the org-credential exposure class: resolved
  header values live in host-agent memory and in node-local `.egress` files
  (mode 0600) for the session's life (ADR 0111). The 24-hour clamp bounds
  the post-disconnect window.
- Per-event side-effect attribution stays at the session level
  (`integration_principal_id` plus the grant snapshot). Per-request
  credential attribution on observe events is a known gap.
- Multi-header user credentials and GitHub user-to-server OAuth are deferred
  extensions; the facet shape and the wire variant leave room for both.

## Phasing

1. This ADR (Proposed).
2. Coordinator substrate: subject kind, `static_token` bundle kind,
   `PutCredential`, `LookupRedirectFlow`, constraint migration, sweep over
   both kinds.
3. Wire + session boot: `OauthUser` variant, wire 28, resolve arms, clamp.
4. Orchestrator: facet parse, grant scope, compile emission, launch gate.
5. Routes: `/api/v1/me` connector credentials, user authorize route, shared
   callback dispatch, Slack surfacing.
6. Web: credentials page cards, start-screen gate, profile toggle.
7. Connector adoption (`linear`, `slack`, `sentry` or `notion`, `github`) and
   flip to Accepted.
