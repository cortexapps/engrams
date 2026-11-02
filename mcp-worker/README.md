# Restricted MCP worker

This package runs remote HTTPS MCP operations in a separate Node 24 service.
The official SDK is pinned to 2.4.0. The image pins the maintained Node 24.22.2
release. The application and each zone keep dispatch disabled by default.

```sh
npm ci --ignore-scripts
npm run format:check
npm run typecheck
npm test
npm run build
```

Tests use Node 24, OpenSSL, and Helm. Test files run serially because schema
checks have hard process budgets. Fixtures use local TLS listeners only. The
production entrypoint has no loopback bypass or alternate transport setting.

## Internal contract

`src/index.ts` exports the pure DTOs in `contracts.ts`, runtime request validation,
and canonical JSON/hash helpers. These modules do not import the application
store, SDK, network service, or credentials. Hashes use UTF-8 canonical JSON:
object keys sorted with JavaScript string order, array order retained, scalar
encoding from `JSON.stringify`, omitted undefined object properties, and SHA-256
hex output. Non-JSON values are rejected. Callers in other languages must use
matching canonicalization; token-grant field names are fixed ASCII strings.

The service accepts only mTLS on port 8443. A caller must present a certificate
signed by the configured caller CA and match an exact SHA-256 certificate pin.
The server certificate also needs the service DNS name and server-auth usage.
The client must check that name and its operator-provided CA. No public ingress,
bearer fallback, generic proxy, or guest listener is provided.

- `GET /identity` returns protocol version, boot ID, zone ID, policy revision,
  and effective policy digest. It requires an approved client certificate.
- `POST /rpc` accepts a versioned `WorkerRequest`. Orchestrator certificates can
  discover, execute, validate schemas/destinations, and cancel. Coordinator
  certificates can request metadata, exchange tokens, validate destinations,
  and cancel. Each operation is fixed; no caller-selected method or header set
  is forwarded to a provider.
- Execution scope contains the real session, run, tool-call, and immutable
  binding hash. Administration scope contains the trusted request and actor
  identities. Execute requires execution scope. Discovery, schema validation,
  destination validation, and OAuth require administration scope. Cancel must
  match the original scope, caller, connection revision, and credential identity.

Before acquiring or sending a provider credential, the trusted client must
verify the worker identity against the approved zone and frozen policy digest.
It must authorize current platform state and acquire durable global admission.
Then it creates a new intent bound to that worker boot, policy, connection
revision, credential identity, operation, exact destination, scope, expiry,
and admission lease. The immutable journal `requestDigest` is separate from
`operationDigest`, which hashes the complete transient operation, including its
credential. A routine token refresh can change the latter without changing the
binding or journal identity. Do not persist the transient operation or token.

The worker consumes each intent before I/O and rejects reuse, expiry, wrong
boot/policy, changed payload, or exhausted local admission. Responses echo the
boot/policy identity, full credential-free intent (including lease expiry and
scope), intent ID, both digests, and lease ID for verification.
A worker restart changes the boot ID. It does not authorize a write retry.
The worker cannot verify database state: only the trusted caller can issue
admission and authorization. It has no database or KEK access.

Discovery returns upstream DTOs, not a published catalog or a permission grant.
Schema and name fields are retained exactly. Descriptions are untrusted text.
Each input, output, and constraint document is compiled in a separate Ajv
instance inside a disposable process, with its own root and dialect. Independent
constraint documents are never wrapped in a new `allOf` root or URI registry.

## Runtime bounds

Every dial validates the exact canonical HTTPS authority and every A/AAAA answer,
then pins one checked address while retaining TLS SNI and HTTP authority. Public
zones reject private and reserved addresses. Internal zones permit only exact
operator hosts within approved private CIDRs. Permanent metadata, loopback,
link-local, multicast, and unspecified ranges cannot be enabled by a zone.
Operator administration denies apply in both modes. OAuth endpoints need their
own approved zone; a private MCP zone does not approve a public OAuth authority.
No redirect, automatic OAuth refresh, or alternate separate-SSE transport runs.

Each operation owns its SDK client and credential. Legacy sessions never cross
operations, platform sessions, or credential identities. The worker accepts the
three approved protocol versions and JSON/SSE replies. It rejects unsupported
capabilities and content and does not fetch icons, links, resources, or prompts.
It uses low-level SDK requests, blocks synchronous SDK schema compilation, and
permits at most one tool POST per intent, including on protocol or auth errors.

Limits include 256 KiB arguments, 1 MiB catalog and normalized output, 2 MiB
**decompressed** HTTP bodies, 128 tools, 128 pages, 5 seconds per connection,
60 seconds per tool by default, and a 300-second ceiling. Outer deadlines and
abort signals cover all I/O. Local active caps are 4 per session, 8 per
connection, and 32 executions per worker. Discovery has two slots, OAuth four,
and schema/destination administration four. Schema compilation also has a
shared four-child cap, including children started by execute/discover. These
local caps supplement durable admission across replicas; they do not replace it.

Schema children have a 128 MiB heap from launch and an empty environment.
Trusted module startup has a separate five-second bound. The parent starts a
hard one-second compile/validation timer **before** sending any schema or data.
No schema enters the child during startup. Deadline, cancellation, process
failure, excess complexity, unsupported dialects, remote references, and unknown
formats fail closed. There is no schema retry or executable custom format.
The Node permission model is not used or claimed as a network sandbox.

Execution returns an upstream outcome separately from output delivery. A valid
completed response can establish success even when unsupported content,
normalization, size, or output-schema checks prevent delivery. Malformed JSON,
a lost reply, or an HTTP binary reply after a dispatched write leaves the
outcome unknown. Neither a guest error nor cancellation proves that a provider
write failed. Cancellation aborts active I/O; it cannot undo external effects.
Client disconnect and shutdown also abort active operations. The trusted caller
must persist verified results and handle late results and unknown outcomes.

Known literal credential echoes are redacted from text and structured output.
A discovered name or schema that contains a known secret is rejected, not
rewritten. Error responses contain fixed codes only; raw SDK errors and causes
never leave the boundary. Token-exchange replies go only to coordinator callers.
No provider payload is logged or stored on disk. This does not promise protection
against encoded echoes or a malicious server that already received a credential.

## Deployment

`docker/mcp-worker.Dockerfile` builds only this package. The Helm chart has one
Deployment, ClusterIP Service, ConfigMap, ServiceAccount, and NetworkPolicy per
zone. See `deploy/helm/engram/values-mcp-worker.yaml.example`. Provision separate
worker namespaces and RPC TLS secrets before enabling them. Workers get no
service-account token, workload-identity annotation, host mount, DB credential,
KEK, or application environment. They run nonroot with all capabilities dropped,
RuntimeDefault seccomp, a read-only filesystem, and core dumps disabled. Only
the operator config and scoped RPC certificate secret are mounted read-only.

Public policies use fixed public CIDRs with reserved, metadata, and operator
administration exclusions. Internal policies use operator-supplied CIDRs and
exceptions. Startup validates each rule and refuses missing or invalid exclusions.
DNS is limited to the selected DNS namespace and pods; ingress requires both
the application namespace and trusted component selector. Each zone has its
own identity, service, and policy. Zone policy changes restart workers and change the
policy digest. There is no automatic authorization expansion.

Kubernetes policies are additive. Operators must verify that no other matching
policy widens these routes, that the CNI enforces them, and that service NAT does
not bypass the intended exclusions. Rendered YAML and local TLS tests do not
prove effective cluster isolation or Firecracker guest isolation. Those release
gates remain required before dispatch can be enabled.

The coordinator credential API uses `engram.mcp.v1.CredentialControlService` on
the pinned mTLS listener. Its status responses wrap `CredentialStatus`; a missing
status is an error. The credential API remains limited to 32 KiB. The separate
`engram.mcp.v1.SessionPolicyService` uses a 4 MiB create budget on that same
listener. Neither service is on the application listener or in its reflection.
