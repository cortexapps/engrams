# Shared stack runner context, version 1

Status: complete source assembly, pending independent review and real platform
execution. The runner and all twelve driver families are implemented under
`orchestrator/scripts/mcp-stack`. The required-case manifest and unit tests are
not physical results. Platform adapters must not treat them as acceptance evidence.

The proposed command is:

```sh
bun orchestrator/scripts/mcp-stack/run.ts --context /owned/run/context.json --matrix all
```

The parser requires both flags exactly once. It accepts only `all`, which selects
12 cases for Claude and the same 12 for Codex. A result cannot pass if a case or
required subassertion is absent. The entrypoint validates the exact driver set,
context, fixed supervisor controls, scoped database schema and both fixtures
before publication or signup. Public enabled-image and enrolled-host checks
precede all cases. It stops on the first failed case and leaves incomplete
evidence. A total watchdog exits 124; the adapter must tear down owned services
on every exit. Do not activate physical CI until the assembly passes review.

## Context fields

`ContextSchema` in `context.ts` is the exact schema. It rejects unknown fields.
The context is a private regular file directly inside a private, real run root.
No component can be a symlink. All referenced files, artifacts, outputs and the
supervisor socket belong to that root and current OS user. The loader hashes
actual artifact bytes. Secrets use private file references. Outputs must not
exist before the run, and use exclusive creation. No default endpoint, cookie,
API key, environment file, image, or primary-checkout path is used.

- `version`: 1; `runId`: a neutral safe identifier; `runRoot`: absolute real path.
- `backend`: `firecracker` or `vz`; `sourceCommit`: exact coherent Git SHA.
- `hostId`: actual enrolled UUID; `imageId`: public enabled-image ID;
  `imageDigest`: actual `sha256:` OCI manifest digest.
- `artifacts`: exact `{path,sha256}` for coordinator, orchestrator, worker,
  hostAgent, fixture, kernel, guestImage, claudeHarness, codexHarness, modelCli,
  claudeBundle, codexBundle, helper. File hashes use bare lowercase hex.
  These identify bytes; they do not prove that a guest ran them.
- `endpoints.app`, `coordinator`, `operator`, `workerControl`: canonical HTTPS origins
  under `.fixture.example`, CA file/hash, uppercase colon-separated leaf SHA-256
  fingerprint. TLS still verifies the CA and DNS name. Operator additionally
  needs a separate certificate file/hash and private key file. No redirects or
  resolver overrides are installed by the clients.
- `protocolFixtures.modern` and `protocolFixtures.legacy`: separate private run
  roots, immutable `configuration:{path,sha256}`, data/control identities, resource key and observer token files; exact
  resource, issuer, authorization, token origins; exact callback URL on app
  `/integrations/mcp/callback`. Modern is `2026-07-28`; legacy is `2025-11-25` or
  `2025-06-18`. Both use the standalone fixture contract, and remain alive across
  all other role restarts. Fidelity subassertions name both actual protocols;
  other cases report only the protocol they actually exercised.
- `publication`: exact configured candidate identity, expected generation (null
  for new deployment), deployment/network/trust identities and zone. Identities
  have `sha256:` prefixes. The generated operator gRPC client sends only the
  configured identity and expected generation, then reads actual publication.
- `identities.admin`, `owner`, `other`: distinct neutral email, password file,
  new cookie output file. `unattendedKeyFile` is a new output for a real public
  role=user service key. `automationKeyFile` is a separate new private output
  for the role=admin service key that triggers actual manual automations. It
  cannot replace the user key in denial cases. `routerSentinelFile` is a neutral input; each protocol fixture has its own resource key.
- `supervisor`: private Unix socket plus separate token file.
- `observer`: separate private coordinator and orchestrator DSN files. Each
  points to a fixture DNS host and database `mcp_<runId with hyphens changed to
underscores>`. Only fixed read-only queries are implemented. The two services
  can use different database servers with that database name. `hostManagedRoot`
  is the actual host-agent `work_dir/managed-boots` directory, owned and mode
  0700. `hostLogFile` is the egress proxy daemon tracing log, owned regular
  mode 0600 with one link. Both are direct paths below the run root, not worker mounts
  or adapter-created observation files.
- `guest`: owned `/tmp/mcp-stack-fixture` directory and absolute immutable model
  CLI path ending in `/engram-mcp-model-fixture`. The image/init owner prepares
  this directory as 0700 before CLI launch. The runner cannot create it after a
  CLI has already read its startup configuration.
- `deadlines`: request 100–30000 ms, step up to 120000 ms, total up to 7200000 ms;
  request <= step <= total. `evidenceFile`: new private output file.
  The required TLS probe needs request >= 6000 ms for its fixed five-second
  OpenSSL deadline and result framing.

Before any admin mutation, the final runner must check context artifacts,
supervisor identities/network enforcement, fixture readiness and full source
prerequisites. Signup/login requires the normal isolated password auth mode.
The real public WhoAmI response must establish three distinct human principals
and distinct user/admin service principals. For unattended runtime cases, the
normal public AutomationService creates a manual definition with one
create_session block and an empty initial prompt. The admin service key triggers
RunNow; GetRun supplies actual task/session IDs. Human admin definition author,
service trigger principal and runtime automation actor are recorded separately.
The runner checks Started and guest readiness before the first marked prompt. The normal OrgSecret PutSecret and
migrated router model path provides neutral model auth. No provider refresh is
called by this runner. The launcher must deny provider routing and external DNS
for O, coordinator and worker before boot and across restart.

## Fixed supervisor interface

POST `/v1/control` over the private Unix socket. Bearer token comes only from
its file. Request is exactly `{version:1,runId,operation,role?,configurationId?}`. Operations are
`status`, `restart`, `stop`. Roles are `orchestrator`, `worker`, `coordinator`,
`host-agent`, `fixture-modern`, `fixture-legacy`. The runner can inspect fixture
status but cannot stop or restart either fixture. The adapter owns fixture cleanup after the runner terminates.

Response is `StatusSchema` in `supervisor.ts`: exact run/backend/source/host
identity, exactly six role entries `{role,state,bootId,artifactSha256,configurationId,configurationSha256}`, and
`network:{generation,enforcementSha256,fixtureOnlyEgress:true,
externalDnsBlocked:true,roles:[orchestrator,coordinator,worker]}`. A restart changes
only the selected role boot ID, preserves the other five boot IDs and the exact
network identity, and returns after roles run again. Status does not substitute
for public enrollment, actual Started, outbox ACK or guest proof.

The adapter computes network evidence from actual installed routes, firewall and
DNS policy. The hash is not itself enforcement proof: physical acceptance must
also observe explicit guest probe denial and the corresponding network event.
No arbitrary command, URL, environment override, SQL, process ID or proxy is
accepted. Only the orchestrator has `global-disabled` and `zone-disabled`
configuration IDs. Global changes only `MCP_ENABLED` to false and retains its
runtime configuration. Zone keeps the global flag true and changes only the
matching configured `workers[].enabled` to false. Both fixed files/hashes are
required before the first mutation. Other roles accept only `normal`; their
configuration and process boot identities remain unchanged during these kills.
The adapter validates byte-identical destinations, CIDRs, CAs, identities and
all settings other than the selected enable switch. Status attests the selected
configuration ID and hash. A missing variant fails the prerequisite.

`supervisor.operatorCandidates` contains only `normal` as
`{file:{path,sha256},identity}`. The runner stages that exact configured identity
once at bootstrap. Global/zone kill and restoration restart only the
orchestrator and read the actual operator publication before and after. Its
identity, generation and admission state must remain unchanged. Neither kill
publishes a new candidate. Coordinator Zone and OperatorCandidate do not have
enable fields, and the runner does not invent one.

## Read-only observations

The runner records task/session IDs only after real public creation. Each query
requires an owned session. Coordinator queries join the exact managed creation
(task ID), selected host and boot; a separate query reads only outbox attempts,
relay status and `acked_at` for `tool_result:<session>:<call>`. Orchestrator queries
check actual session listener ownership for the same task/session. Pools set
`default_transaction_read_only=on`, one connection and bounded statements. They
never select credentials, raw arguments/results or mutate state. O managed
lifecycle/Started observation still needs the accepted producer schema after
coherent integration; listener lease alone does not establish Started.

The public journal result and its settlement field, fixture effect count,
coordinator outbox ACK and actual guest proof are different observations. A
settled-or-target-gone value cannot be called guest delivery. Only actual guest
result proof plus actual ACK establishes that subassertion. The evidence sink
records bounded states, counts, sizes and hashes; it refuses unknown payload
fields and case results that cite observations not collected for that case.
It does not print exceptions, credentials, cookies, codes, arguments or results.

## Required unresolved seams

1. The runner consumes the separately owned additive CLI contract: bounded
   scalar arguments/call IDs/native expected result, or an immutable fixed call
   plan with explicit prompt markers. Plans exclude scalar settings. Actual
   exposure name-set hashes and native response hashes are verified. The CLI
   source must be integrated before physical runs.
2. Guest proof files are JSONL, one flushed completion per line, including
   multiple turns in one process. Each record is validated and hashed separately.
   Repeated markers retain call IDs; each prompt attempt uses a distinct prompt
   ID so the public prompt deduplication layer cannot replace the replay probe.
3. The compiled alias can be derived from the actual public connection ID/alias
   and upstream name, then must match paginated public GetSessionMcpState before
   prompting. No description-based selector or guessed alias prefix is needed.
4. Modern and legacy MCP protocol is fixed for a standalone fixture lifetime.
   The context now requires two independent fixture instances. The adapters
   still need to implement both real lifetimes.
5. No public global/zone flag setter exists. The fixed orchestrator-only
   configuration contract above uses the actual startup switches. Adapters
   must implement it without changing operator publication or network trust.
6. Child grant intersection needs an actual native child-task invocation.
   The separately owned fixed plan supports an exact native spawn step and a
   child message marker. Its implementation must be integrated first.
   Forged guest completion and direct bypass probes need actual guest transport,
   not a human owner's public call labeled as guest traffic.
7. The fixture OAuth control contract adds future-issued `expiresInSeconds`
   (integer 1–3600; default 3600), observed under `oauth.expiresInSeconds`. The
   runner uses 2 seconds and the actual coordinator clock. Existing token
   deadlines do not change when the configuration changes. This separately
   owned fixture extension must be integrated before a run.
8. Coherent production startup and both physical adapters remain external work.
   No helper module or unit test in this directory establishes physical proof.

The fixed call plan can assert `expectExposure: "absent"` for an exact MCP
alias. Such a step must observe a real protocol denial and cannot include an
expected result or premature completion. `completionProjection: "premature"`
is a separate adversarial step for an exposed MCP call and cannot include an
expected result. Barrier records are observations, never result receipts.

Acceptance profiles freeze `ENGRAM_FIXTURE_PROOF` with a single `{launch}`
token. The CLI expands it to its decimal PID and a fresh 128-bit OS-random
nonce, separated by a hyphen. Discovery accepts only
`<scenario suffix>-<PID>-<32 lowercase hex>.json`. It has no PID-only fallback.
Exclusive creation preserves every old proof across disk recovery even when
the new guest reuses a PID. Nonces are observation filenames, not authority.
The CLI implementation of this additive token must be reviewed and integrated
before physical execution; legacy literal and `{pid}` modes are not used here.

The common runner exposes only two release files under the fixed guest state
root: `completion-release` and `premature-release`. Profile options enable the
matching fixture environment variables. Releases use exclusive creation with
mode 0600. The CLI independently validates and consumes each release. Its fixed
120-second bound is not configurable by the runner. A normal native completion
notification still requires separate actual coordinator outbox ACK evidence.

Managed resume uses the public `PrepareMcpRebind` and `RebindMcp` review flow.
The runner checks a new sandbox and boot attempt, the next binding epoch, exact
reviewed bindings, coordinator and orchestrator Started state, and guest return.
Ordinary `SessionService.Resume` must remain denied for this managed session.

A call ID may occur in exactly two ordered plan steps for the rebind test:
first present, then explicit absent. Both steps must use the same MCP alias,
canonical arguments, and MCP result format. Neither can project a premature
completion. All other duplicate IDs fail. The runner counts prior proofs before
recovery and requires a new explicit absent-denial proof after the second step.

Delivery checks combine the actual native result proof, public
`tool_call_completed` event, and coordinator outbox ACK with the current managed
delivery target. An outbox ACK alone is insufficient: the coordinator also ACKs
an old delivery after managed cold recovery retires its target.

## Physical observations and platform prerequisites

The runner reads the sealed host `<sandbox UUID>.boot` record directly. It
requires schema 1, an exact body hash and sandbox identity, a version 1 record,
and the current creation/source key. Reads use no-follow file descriptors,
0600 owner-only single-link files and stable descriptor/path metadata. Envelope
and body bounds are 16 MiB and 8 MiB, with depth at most 64. The completed
capture authority must identify a destroyed source and exactly match the new
cold request. The runner hashes the actual `<capture UUID>.disk` bytes with a
16 GiB fixture limit and the step deadline. That limit is not a product limit.

An exact, read-only coordinator query joins the owned task/session, current
boot attempt and completed managed boot intent. It selects only the nonsecret
asset receipt, start intent, native bytes and combined manifest. It never reads
the sealed agent or credentials. The host Started receipt and incarnation must
match the coordinator start intent and selected backend, kernel, disk and
auxiliary drives. The actual guest harness environment must contain the exact
combined manifest hash, with the unchanged native prefix. Before and after cold
recovery, the guest supplies its real kernel boot ID. An exclusive disk
sentinel must survive, while a sentinel on verified `/dev/shm` tmpfs must be
absent after recovery. This checks loss of the old tmpfs state; it does not
claim that every physical RAM byte was erased.

The platform adapter must separately establish actual hypervisor availability,
the exact owned process and executable for the selected backend, immutable
source/configuration hashes, and the real guest transport. The managed intent
receipt proves which backend the authenticated production host path selected.
It does not replace that platform evidence. Configuration strings or supplied
success booleans do not establish either layer.

For managed-destination denial, the runner records the proxy daemon log inode
and byte offset, issues a fixed guest TLS probe to the configured resource
origin, then requires a new exact proxy Reject event for the owned session and
SNI. The log must not rotate or truncate. The reader allows at most 8 MiB of
new bytes and 64 KiB per line, and consumes only newline-terminated records.
Only hashes leave the reader; raw logs are not evidence artifacts.

`endpoints.workerControl` must name the actual private worker control origin.
The runner makes a TLS 1.3 connection from the trusted fixture network to the
worker control endpoint. It verifies the CA, hostname and exact server leaf,
without a client certificate or an HTTP request. A certificate-required alert
116 is classified as `mtls_authentication_denial`. Timeout, network failure,
close and other TLS errors cannot pass. This verifies the worker's authentication
boundary; it does not establish guest network isolation.

Adapters must provide both independently owned protocol fixtures, private
databases and normal public auth, actual enrolled host/image/assets, the fixed
supervisor variants and candidate files, the actual host record/log paths,
guest tools/state directory and fixture CLI, and the fixed worker control
endpoint. They own start/stop/reap and cleanup of every launched resource.
The source runner does not build images, launch services, migrate databases,
install routes, mint host credentials or create platform evidence.
