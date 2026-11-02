# ADR 0125 — Isolated external MCP tools

Status: Proposed (2026-11-01)

## Context

ADR 0058 deferred native MCP configuration in the guest. That design lets a
harness connect to an upstream server and does not provide a platform write gate.
External tools must instead use the authenticated ENGRAM_TOOLS rail from ADR 0089. Claude uses its existing bridge; Codex uses its existing dynamic tools.
Both use parked calls and the durable completion outbox. MCP metadata is
untrusted input, never a new process-global tool registry.

ADR 0106 is the credential authority. MCP OAuth uses subject kind `Mcp` and the
named integration connection ID as subject ID. Organization API keys use the
existing KEK-sealed organization secret store. There is no parallel secret store.
Personal credentials, guest stdio servers, package installation, resources,
prompts, sampling, elicitation, MCP Apps, and asynchronous Tasks are out of scope.

## Decision

### Service and trust boundary

Add a trusted standalone Node 24 ESM service, `mcp-worker`, with the official
`@modelcontextprotocol/client` pinned to 2.4.0, Ajv pinned to 8.20.0, and a
maintained IP parser. Its mTLS interface has fixed discover, execute, cancel,
OAuth metadata, and token-exchange operations. It has no generic proxy or guest
listener. Each operator zone has a separate deployment. It receives no database
credentials, KEK, cloud identity, host mounts, or service-account token. Disable
core dumps and credential disk writes. Credentials exist only in operation memory
and must never appear in logs.

The orchestrator issues expiring, single-use intents bound to worker boot ID,
connection revision, exact destination, operation, and call digest. Discovery and
OAuth have separate budgets. The worker cannot authorize a tool call or refresh a
credential on its own. The coordinator is the only credential and refresh
authority. The worker receives secrets only after admission, near dispatch.

The coordinator exposes credential control only on a separate, default-off mTLS
listener. Exact client certificate pins identify the trusted orchestrator role.
The existing cleartext application gRPC listener never serves this service. The
orchestrator may hold a short-lived credential lease in memory for one admitted
worker operation. It must not queue, cache, log, persist, or return the value to a
guest or browser. Resource calls use the orchestrator worker role; OAuth metadata
and token calls use the coordinator worker role. A lease checks the actual sealed
identity, config revision, and authorization generation, including when publication
in the orchestrator database failed. Worker boot and operator policy must match
the frozen operation; a new identity lookup cannot replace a frozen boot identity.
Worker boot binds an operation, not durable credential authority. After a worker
restart, fresh operations can use the current boot with the same sealed authority.

The credential delivery deadline is at most ten seconds. The separately frozen
operation deadline is at most five minutes and remains the worker deadline.
Admission must cover that full operation deadline. Credential control echoes both
deadlines; the orchestrator validates them and dispatches before delivery expires.
Status returns NotFound only for absent authority metadata. Transport and service
failures remain unavailable and cannot justify recovery or replacement.

### Reviewed catalogs and session binding

Reuse named integration connections and profile integration grants. Discovery
creates a draft. Only administrators can publish an immutable reviewed catalog,
using expected-revision compare-and-swap. Preserve upstream descriptions,
schemas, and fingerprints. Administrators assign explicit read/write classes;
server annotations confer no permission.

An immutable task-session binding includes connection/config revision, credential
identity, authorization generation, catalog revision, grants, constraints, and
hashes. Tool names resolve only through that binding. The guest cannot provide
endpoints or credential subjects. Changes to endpoint, account, authentication,
policy, or reviewed tools invalidate the binding. A normal refresh with the same
account and scope preserves it. Rebind must be explicit and cannot widen an
existing session silently. Child grants intersect parent effective grants. Plan
mode permits only reads and comes from trusted state.

Managed upstream authorities and aliases enter the guest deny set before any
allow or default-allow rule and before credential injection. Reject conflicts
with direct integrations. MCP secret references cannot use generic guest inject
or mint paths. Keep old denies when invalidating; install new denies before a
rebind becomes visible. This guarantee covers organization-managed credentials;
unknown alternate origins and user-supplied credentials are outside it. Upstream
servers must authenticate writes.

### Network containment

Accept canonical HTTPS URLs with exact approved authority and port (443 by
default). Reject userinfo, query, fragment, IP literals, and redirects. A public
zone denies private and reserved ranges. An internal zone allows only exact
operator-approved hosts and private CIDRs, with operator-approved CAs. An
administrator cannot widen CIDRs, CAs, or routes. Every zone denies loopback,
link-local, metadata, multicast, unspecified, and platform administration
addresses. Apply this policy to OAuth as well as tools.

Resolve every A and AAAA record on each new dial. Reject mixed permitted and
forbidden answers. Parse addresses with an established library and pin the checked
IP while preserving TLS SNI and HTTP authority. Local loopback and private test
CA allowances exist only in fixture code, never in production policy.

### Protocol and validation

Support only Streamable HTTP with JSON or request-scoped SSE responses, using
modern 2026-07-28 and legacy 2025-11-25 / 2025-06-18. Modern requests have no
initialize handshake or transport session. Legacy sessions are scoped to platform
session and credential identity. Never fall back to the deprecated separate SSE
transport. Untrusted responses cannot trigger resource, link, or icon fetches.

Preserve text, structuredContent, and isError. Reject binary and resource content.
A delivery failure is distinct from the upstream outcome. Support JSON Schema
Draft 7 and 2020-12, local references only, bounded complexity, and no unsupported
dialects, remote references, or executable formats. Disable coercion, default
insertion, and property deletion. Compile and validate in a disposable child with
a hard one-second deadline and 128 MiB heap.

### OAuth

Use pre-registered public or confidential clients, no dynamic client registration.
Require PKCE S256, encrypted durable single-use state with a ten-minute expiry,
exact redirect URI, issuer validation, and resource/audience checks. Independently
approve metadata, authorization, and token destinations. Require explicit admin
consent to share the organization-session account.

Use single-flight refresh and rotating-token compare-and-swap. Do not fall back
to stale credentials or expand scope silently. Send access tokens only to the
resource; send client secrets and refresh tokens only to the token endpoint.

MCP redirect state is sealed in the existing OAuth flow store and consumed
atomically before token exchange. Coordinator OAuth requests use durable admission
with four concurrent slots per deployment and one per connection. Refresh claims
are exclusive and version-fenced; a token deadline ends before its claim. An
uncertain rotating exchange leaves the credential unavailable until reconnect.
No expired-claim retry may reuse the previous refresh token. Ordinary refresh
preserves authority identity; reconnect and API-key rotation create a new identity
bound to the next expected authorization generation. Before switching credential
kinds, references, or registrations, the orchestrator serializes the connection
transition and revokes the old authority with its exact identity and version. It
verifies the durable revocation receipt before acquisition and publication of the
replacement. Failed publication cannot restore the old authority.

The existing connection row holds only credential metadata and a durable transition
token. A transition reserves its final config revision before the coordinator seals
a replacement. Publication uses that same revision and compares the transition
token and authorization generation. Recovery changes the token and leaves old
authority revoked. Late control replies and callbacks cannot publish through an
old token. A lost fresh-reference response can leave a sealed orphan; recovery
cannot publish an unknown reference or restore the previous credential.

A local transition token alone cannot fence a delayed coordinator callback.
Before recovery or replacement, the orchestrator cancels each recorded attempt
by exact flow UUID, MCP connection and registration. Cancellation clears sealed
state and marks the existing flow row cancelled. Callback consumption admits token
I/O; cancellation before consumption prevents that exchange. Credential publication
locks the same flow row, requires live consumed state, applies credential-version
CAS and marks success in one transaction. Cancellation after consumption therefore
fences publication even if external I/O cannot be stopped. If publication wins,
recovery queries and revokes that exact credential before clearing its local fence.
A lost begin response can leave an unknown pending flow until expiry and cleanup;
the stale orchestrator call cannot return its authorization URL or record a usable
callback after recovery. Unknown flow IDs are never inferred or broadly cancelled.

### Authorization and irreversible dispatch

Before execution, validate the active session/run, immutable binding, principal,
ownership, live permissions, current revisions, immutable grants, tool input, and
admin constraints. Writes also need one-use approval or an explicit automation
grant. Owner/admin interactive approval expires in ten minutes and binds the exact
session, run, call, connection, catalog, generation, and canonical argument hash.
It cannot expand grants or be replayed. Automation-plan approval cannot approve
MCP writes. Unrestricted automation constraints `{}` must be selected explicitly.

Keep the authoritative call journal separate from the UI pending projection.
Use unique `(session_id, tool_call_id)` and immutable request digests. Enforce
`waiting_approval -> authorized -> dispatching -> succeeded/failed/canceled/outcome_unknown`
with explicit legal transitions. Entering `dispatching` is the irreversible write
barrier: never auto-replay a write after crash, lease loss, or timeout. External
effects stay outside retried DBOS steps. Only reviewed safe reads may have bounded
retries. Persist a verified result before the completion outbox obligation.

An unknown write blocks later writes for the session/connection, even with new
call IDs, until an owner/admin resolves it. Established success with unusable
output stays `succeeded` with result unavailable; otherwise record outcome unknown.
Guest completion projection is not evidence of upstream success. Stop, interrupt,
revoke, and deadline cancel active I/O best effort; cancellation cannot undo an
upstream effect. Resume uses live policy without widening immutable grants and
retains durable late results.

### Limits and audit

| Limit                                          | Value                               |
| ---------------------------------------------- | ----------------------------------- |
| Connections / tools per session                | 16 / 128                            |
| Manifest / arguments / normalized result       | 1 MiB / 256 KiB / 1 MiB             |
| Decompressed response                          | 2 MiB                               |
| Connection / tool deadline                     | 5 s / 60 s (operator ceiling 300 s) |
| Concurrency: session / connection / deployment | 4 / 8 / 32                          |
| Queue size / expiry                            | 128 / 30 s                          |
| Pending approvals: session / deployment        | 16 / 128                            |
| Schema validation deadline / heap              | 1 s / 128 MiB                       |

Enforce connection and deployment admission across replicas. Audit metadata only:
identities, revisions, decision, actor, call, timing, sizes, and outcome. Never audit
raw arguments, results, headers, or secrets. Session payloads use existing retention
and access rules. Redact known secret echoes, without claiming protection against
a malicious credential recipient.

Use additive indexed foreign-key migrations, unique IDs, and state constraints.
Never alter applied migration checksums. Version shared protocols. Coordinator
persistence changes require simulator and real-Postgres conformance tests.

## Rollout, rollback, and acceptance

All global, zone, connection, and tool flags default false. Each kill switch blocks
new dispatch and cancels active work while preserving journals. Rollback disables
dispatch and drains work; it never deletes live history or migrations.

Stage rollout: public reads with API keys, private endpoints, OAuth, interactive
writes, then explicit automation writes. Before enabling any stage, run its hostile
fixture cases, current-head CI, and operator acceptance. Both Claude and Codex must
pass the shared protocol path, approval, denial, cancellation, eviction/resume,
secret containment, and unknown-outcome gates. Require real Firecracker production
acceptance. VZ proves local parity separately and cannot satisfy that gate. Fork
runners are not configured yet; runner provisioning is an external prerequisite.

The foundation adds only a test fixture and package scaffolding. It does not enable
worker policy or authorize dispatch. The baseline QuickJS deep-recursion failure is
recorded in `docs/testing/external-mcp-baseline.md`; do not weaken or skip it.
This record stays Proposed until the final acceptance review records evidence and
the implementation commit chain.

## Sources

- [Streamable HTTP 2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)
- [Official TypeScript SDK 2.4.0](https://github.com/modelcontextprotocol/typescript-sdk/releases/tag/v2.4.0)
- [MCP authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)
