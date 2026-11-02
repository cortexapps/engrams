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
listener. The internal service name is `engram.mcp.v1.CredentialControlService`.
Its four status operations return method-specific response wrappers with one
`CredentialStatus` field. Clients reject a missing status. The unreleased service
has no alias for the earlier name. Exact client certificate pins identify the trusted orchestrator role.
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

Managed policy has a durable monotone host union. The coordinator saves the
desired union in the session integration policy before host I/O. The host saves
the desired state before proxy application, drains existing session connections,
then saves an applied marker before ACK. Read and compare-and-apply bind the exact
session, sandbox, physical epoch, host and proxy incarnations, binding hash,
operator identity, revision and digest. A lost ACK cannot remove retained hosts.

The trusted read returns retained coordinator state separately from the exact
host receipt. A required wrapper carries the checked physical read target and
an optional canonical retained policy; a missing wrapper is unsupported, not an
empty union. Consumers retain both policies and choose a revision above both.
Coordinator persistence is not proof of host application. Reads check placement
before host I/O and after reading the retained policy, without a transaction
across that I/O.

Managed host mode requires mTLS for the whole host mutation listener, exact
coordinator certificate pins, and host-specific server identity verification.
There is no simultaneous plaintext mutation listener or downgrade fallback.

A managed host owns the durable policy directory with one lifetime exclusive OS
lock. The lock file keeps its inode and is never removed during handoff. The
lock releases on process exit or crash. A successor quarantines every managed
session reported by the surviving proxy before it reads durable records or
replays ordinary policies. Missing or corrupt records fail startup and keep
admission closed. Valid replay checks the physical target and live network
capability before it produces a new receipt for the successor incarnation.
Managed control requests use separate local proxy streams; a session drain does
not hold a global network lock. Directory ownership requires a persistent host
volume and one managed host generation per directory in deployment.

Generic start checks all durable managed session associations for the actual
sandbox before any bind or policy write. A duplicate ordinary binding cannot
hide a managed record, including one retained after unbind. Supplied fence and
policy identities must agree with the requested sandbox; the explicit unfenced
sentinel remains available to ordinary legacy callers.

Until the strict restore producer exists, known managed sessions reject restore,
base restore, resume, and agent start. The management marker is trusted host
state, and a corrupt marker does not make the session ordinary. A fresh target
host has no local marker: coordinator classification and pre-restore fencing are
still required before cross-host migration can be enabled.

Fresh create and restore use a durable coordinator pre-start gate at the real
physical binding epoch. The orchestrator pulls preparation state, compiles and
commits a final candidate, then finalizes the exact manifest and host receipt.
The coordinator never holds a transaction waiting for the orchestrator. Initial
prompts remain durable and blocked until finalization. The native-only manifest
is an immutable creation input with authenticated session association. Restore
must quarantine guest execution and network before it can run saved state.

The complete deny superset includes every operator-declared selectable authority
and alias in every zone, including unused zones. A durable operator generation
stages additions, closes new managed boot admission, and requires all affected
sessions to ACK the union or have a proved network fence before publication.
The worker configuration digest and deployment/network identity are separate
inputs. Missing capabilities and mixed generations stay closed. Network policy,
boot finalization, and operator publication are separate implementation slices;
the feature stays disabled until all producers and their composition are verified.

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

### Trusted session write policy

The coordinator owns a durable conservative MCP write-policy latch. This is server
permission, not proof of the guest mode. A trusted plan input sets read-only and
advances the revision in the existing prompt/outbox transaction. A later queued
default or absent directive cannot relax it. This can keep external writes blocked
after a default prompt starts locally. The session state explains the restriction;
the feature stays disabled until the UI presents the required review action.

Only actual new creation over the dedicated pinned mTLS session-policy service can
initialize known policy, inside the existing creation write-set and after validation
of supported harness semantics. Legacy and generic creation remain unknown. The
initial plan restriction is part of creation, not its best-effort initial prompt.
An immutable parent link makes reads and dispatch intersect all current ancestors.
Missing ancestry fails closed. Durable policy survives restart and disk restore;
binding-epoch changes invalidate old request references.

A human owner or administrator can relax a known restrictive policy only by accepting
the exact pending plan exit, bound to the restrictive revision, binding epoch, and
first durable requested-call payload. Changed duplicates, rewound requests, already
submitted/completed calls, and old plan requests cannot authorize relaxation. Fresh
orchestrator identity and ownership checks precede a pinned mTLS acceptance. The
coordinator commits the policy decision, submitted event and existing result outbox
in one transaction. Public CompleteToolCall carries additive expected review context;
without it, ordinary delivery is unchanged and cannot relax MCP policy. Automation,
guest completion, run-start, mode-shaped strings and disk stamps never relax policy.
The session-policy service shares the dedicated listener and client pins, with a
separate bounded create message budget; credential control keeps its 32 KiB limit.

Request acceptance freezes eligibility and exact leaf/ancestor policy references.
An earlier read-only or unknown request cannot gain writes after relaxation. A stale
eligible request cannot rebase. After the orchestrator commits its irreversible
journal barrier, it must synchronously validate those references, the complete
stored ancestry, current binding and execution identity at the coordinator. The
coordinator locks all involved session rows in stable order. Its successful locked
check is the policy-admission ordering point. A restriction before that point blocks
the operation; one after it applies to already-admitted active I/O, canceled best
effort. This is not an atomic transaction across the two databases. Live orchestrator
permissions and journal admission are separate checks. A lost reply or crash after
the journal barrier never permits automatic write replay. Plan approval does not
replace the separate exact external-write approval or explicit automation grant.

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

### Managed network producer limits

Managed sessions inspect otherwise allowed external TLS through the existing
HTTP interceptor. Literal mode does not add credentials. HTTP authority must
match the normalized TLS SNI and port. The proxy rejects opaque protocols,
HTTP CONNECT, upgrades, and chunked HTTP/1 requests that this adapter cannot
bound. Clients must trust the guest proxy CA; certificate-pinned clients and
custom non-HTTP TLS can fail. Ordinary sessions keep their current behavior.
Managed sessions cannot use connector-owned tunnels because the current
connector contract does not prove the external authority. The gateway and TLS
proxy share session connection cancellation and drain.

A proxy acknowledgement alone does not prove complete guest enforcement. The
backend must supply a strict network proof tied to the exact host, sandbox,
physical epoch, guest IP, and network identity before a final binding ACK.
The default backend proof is absent. The existing Firecracker rules redirect
TCP 443 and DNS; they do not establish all-port enforcement. The next producer
slice must install pre-execution host controls for all TCP ports and close
unobserved UDP paths, including restore. Bootstrap receipts have no final
binding hash and cannot authorize tool exposure. Local socket tests do not
establish Firecracker, VZ, or harness compatibility acceptance.

### Private network transport

The selected VZ producer uses one bounded gVisor stack per VM. The helper owns
no host IP listener and cannot dial an external destination. The host passes
three owned descriptors: the Ethernet datagram channel, a reliable parent
control stream, and a registered private proxy stream. A private flow carries
its original IPv4 address and port and an SCM_RIGHTS stream. The Rust proxy
uses the existing session lease, TLS interceptor, and DNS policy. The private
stream is the allocation capability; no bearer credential is stored or logged.
Registration starts closed. An exact placement epoch must be bound before
packet admission. Replacement, policy change, or loss of either reliable
channel closes admission and joins owned work. A transport-ready response is
not a network proof or a final binding acknowledgement.

The helper pins gvisor.dev/gvisor at
`v0.0.0-20260413194555-9680d69bf798` and Go 1.26.2. Ethernet MTU is 1500.
The host selects the exact source MAC, guest IPv4 address, and gateway.
Only IPv4 TCP and DNS at that gateway can reach the private proxy. Established
stack and DNS parsers own protocol decoding. There is no NAT fallback.
Firecracker will use an owned per-interface early fence, all-TCP redirection,
controlled DNS, and drops for other UDP; installed rule ordering still needs
Linux acceptance. Managed recovery will cold-boot from eligible disk-only
artifacts. Saved session RAM and device state are not eligible. Backend
activation, durable boot permits, packaging, and platform acceptance remain
separate work. This decision remains Proposed.

A policy drain retains its session lease until the helper returns cleanup EOF
on the reliable private stream. The helper first closes packet admission and
joins its stack, packet, flow, DNS, and control work. The proxy drains pending
descriptor carriers while it waits, with a five-second limit. A failed barrier
poisons the stable session gate; policy replay or a fresh allocation cannot
reopen it in that proxy incarnation. Recovery needs a fresh proxy incarnation
and full policy replay with quarantine checks. Other sessions remain usable.
This barrier cannot revoke bytes that were delivered before it completed.

### Immutable managed creation authority

The session create transaction stores a held reservation with the exact native
ENGRAM_TOOLS bytes, version and SHA-256 digest. It binds the reserved session ID
to the task, create input, actor and immutable parent policy. The request digest
covers the exact serialized trusted creation request and its native version and
task context. A retry must match all original input; it cannot replace a manifest
with the current profile. Empty native input is an explicit `[]`, never an absent
value. Native classification follows the descriptor contract, not a name prefix.

Held reservations use the existing queued session row and all existing creation
satellites. Ordinary queue listing and placement exclude them. No host request
occurs during reservation. The pinned mTLS policy service alone can read this
source. Public creation and managed admission stay closed until the operator
admission and physical boot gate exist. The future boot transition must claim
the same held row and bind the actual physical epoch; a reservation is not a
network receipt or permission to start a harness. Legacy sessions have no source
and cannot be backfilled from guest state or a current profile.

### Closed cold preparation

A managed cold prepare request names one exact private disk manifest version,
its materialized bytes, its trusted source record, and all selected boot files.
Fresh images first get a private writable disk lineage. Capture eligibility and
disk consistency come from the trusted durable producer, not from guest status
or the absence of a memory manifest. Legacy artifacts cannot supply this input.
The backend pins private copies and verifies their bytes before construction.

The VZ producer owns a fresh stopped machine, its exact file-handle network
attachment, and the helper's reliable channels in one opaque preparation object.
It is outside the ordinary sandbox map and has no start or resume operation.
Its receipt means closed preparation only; it is not an enforcement ACK.
The later durable start producer must bind actual placement, final manifest,
policy and start intent, check current liveness, and integrate all ordinary
lifecycle gates. Drop must terminate and reap the helper without running the VM.

VZ uses a host-selected static IPv4 address and gateway in the same /24,
a fixed unicast MAC on each private point-to-point attachment, and MTU 1500.
Kernel IP configuration also names that gateway as DNS. The current image init
still writes legacy fixed resolver addresses and can preserve a pre-existing
resolver file. A managed init/resolver producer must replace that behavior for
these selected inputs before release, including existing resolver files.
Guest configuration supplies functionality; it is not host enforcement proof.
The backend does not widen the helper's DNS acceptance to old resolver addresses.

The registration API returns an opaque allocation-and-stream owner. The VZ
backend accepts that owner, not independently supplied allocation metadata and
a raw stream. The helper inherits only the three selected descriptors, an empty
environment, and no arguments. Native attachment identity, reliable control
liveness, and child-process state are checked before a closed receipt. A helper
cleanup timeout stays failed after forced reaping. This local cleanup result
cannot replace the proxy's lease barrier or clear its stable session poison.


### Durable managed boot and disk capture authority

The immutable creation row remains unchanged. A companion boot row claims the
actual host, sandbox, physical epoch and boot attempt before host I/O. Each
transition takes the session or policy-ancestor locks and compares its exact
revision. The pinned policy service reads this row and finalizes its manifest;
it never waits for the orchestrator while holding a transaction. Finalization
preserves every original native descriptor and its order. Only the full sealed
AgentSpec is durable; the plaintext environment is a transport-only input.

The prepared receipt names the actual network allocation and host incarnation.
It is closed preparation only. Finalization requires an authenticated current
policy receipt with the final binding hash, the exact native source and the
combined manifest digest. The coordinator commits one start intent before its
first host call. After a lost reply, the scanner reads the same host intent and
state. Missing state does not authorize another start. Scanner pages use stable
session IDs so failed rows cannot starve later claims.
The scanner also reads prepared, finalizing and started owners so a retained
retirement proof cannot be hidden by a failed finalization or start reply. An
older exact retirement receipt may be recorded when the stored desired deny
policy is already a newer superset. This keeps the newer next-boot requirement;
it does not acknowledge that policy as applied to the retired owner.

The singleton `managed_operator_generation_gate` starts closed with no published
generation or identity. Boot claim, finalization, start intent and started CAS
acquire its `FOR SHARE` lock before session or ancestor locks and require an
open exact generation and identity. The authenticated operator publisher must
acquire `FOR UPDATE` first, close admission, stage the generation and invalidate
participant readiness in a short transaction. It may open the gate only after
all required participants have current actual receipts. No host I/O occurs
under either lock. The publisher must also use this lock for held-create
admission and placement invalidation. There is no public or configuration
setter. Public managed creation stays unavailable until that publisher exists.

A cold capture first interrupts the old physical target. It preserves history
and pending prompts, retires pending old tool-result deliveries, and makes old
call targets permanently unavailable. A capture intent fixes its source key,
capture ID, and independent disk manifest (capture UUID, version one) before
host I/O. The intent and claimed disk are GC roots; a missing pending manifest
fails GC collection closed. The authenticated producer returns the actual
retirement and disk receipt. Its digest binds the exact image, kernel, ordered
bundles, disk manifest, byte hash and size, and `CrashConsistentDiskCutV1` result.
That result does not assert clean guest caches or application transactions.

Recovery requires the exact stored capture authority and an authoritatively
destroyed source. A capture-shaped request is insufficient. Destination prepare
also receives the exact record, so it verifies the selected disk against the
same retired source. Same-source capture retries return the same durable result,
including after replacement. No path infers a disk from a file name or restores
memory, device state, or a pending effect. Generic coordinator lifecycle and
execution gates remain closed until the appropriate durable start proof exists;
generic resume, snapshot, teleport, and harness restart cannot replace this path.

### Firecracker closed cold producer

Managed Firecracker uses one anonymous network namespace per preparation. The
namespace has only its loopback device and one TAP. It has no veth, external
route, shared address slot, or host-root firewall rule. A dedicated native
thread creates it; retained namespace descriptors select every later command
and the Firecracker child. Namespace device/inode, TAP ifindex, MAC, IPv4
addresses, MTU, routes and installed rules are read back. Carrier state and
TAP queue count can change when Firecracker opens the same attachment; they
are not attachment identity. Administrative state remains checked.

Before the TAP is brought up, owned netdev ingress/egress and inet
input/output/forward chains drop all traffic. Separate strict chains remain
installed behind that fence. They check the selected source tuple and narrow
ARP exchange, redirect all IPv4 TCP to a namespace-local listener, allow DNS
only at the selected gateway, and drop other protocols. Port inspection is
after defragmentation. No host-root NAT or established-flow rule participates.
The namespace owns its conntrack table; destroying all namespace, socket and
process owners discards it without a host-wide flush or address-slot reuse.

The bounded Rust adapter transfers Unix streams only to the registered private
policy ingress. TCP requires the kernel original destination and port, with
no default-port fallback. Gateway TCP/UDP DNS uses the bounded private DNS
operation. The adapter makes no destination dial. Proxy loss, an invalid
handoff, owner loss or runtime shutdown is terminal. Owned flow tasks stop
before private cleanup EOF. Native child cleanup runs on a reserved thread;
Drop initiates it, while explicit close reports its result. Timeout or failed
cleanup cannot become a successful receipt.

The cold constructor verifies private copies of the selected disk, kernel
and auxiliary files, configures fresh devices, and checks the Firecracker
state is Not started. It does not load memory or saved device state and has
no start, resume or ordinary-map conversion. The closed receipt binds the
exact request, registration, network identity, rules digest and configuration
digest. It does not certify capture consistency or final policy enforcement.
The durable start producer must consume the owner, bind placement and final
policy, then implement the sole release/start operation. Its guest resolver
must use the selected gateway. Root installed-rule, packet and VM acceptance
remain required on Linux; local socket fixtures and cross-compilation are not
that evidence. This decision remains Proposed.

## CLOSED namespace CI qualification

The required Firecracker root lane builds the current library test executable and
runs `managed::network::linux::tests::actual_closed_namespace_readback_detects_route_tampering`
with `--ignored --exact`. The gate requires one selected test and one passing test.
It requires root with CAP_SYS_ADMIN and CAP_NET_ADMIN, a TAP device, `ip`,
`sysctl`, nftables JSON support, and kernel netdev egress support (Linux 5.16 or
later). A missing capability or unsupported kernel fails the test. The lane does
not depend on the optional NBD device check.

This is source wiring until the assigned CI lane runs and records a pass. It checks
installed CLOSED state and detects route tampering. Packet forwarding, original
destination, live DNS, Firecracker VM, VZ parity, full-stack and CNI acceptance
remain separate gates. This check does not authorize activation.
