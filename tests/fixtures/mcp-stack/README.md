# Standalone MCP stack fixture

This test-only Node 24 process reuses the worker's MCP and OAuth fixture. It is
an independent server, with durable effects outside the worker and orchestrator
restart domains. It does not use production credentials or change worker
transport checks. Start it with one absolute config path:

```sh
node tests/fixtures/mcp-stack/main.mts /absolute/owned-run/config.json
```

The launcher supplies the certificates, key, neutral credentials, run directory,
DNS, routes, and process handles. Use one fixture process per run directory. The
launcher must keep the control listener outside guest and worker reach. The
fixture does not create networks or install DNS entries. Local tests bind only
loopback and use ephemeral ports. A future networked launcher can bind an exact
non-loopback IP without a source change.

## Config version 1

All keys below are required, except `control.host` (default `127.0.0.1`) and keys
inside `case`. Unknown keys fail startup. Paths must be absolute, real regular
files. TLS and credential files must be inside the real, private (`0700`) run
root. Symlinks are rejected. Each file has a size limit. Keep the config private.
The JSON below uses neutral example paths and names; a launcher supplies real
values and available ports.

```json
{
  "version": 1,
  "runRoot": "/absolute/owned-run",
  "listen": { "host": "192.0.2.20", "port": 8443 },
  "advertised": {
    "resource": "https://mcp.fixture.example:8443",
    "issuer": "https://issuer.fixture.example:8443",
    "authorization": "https://authorization.fixture.example:8443",
    "token": "https://token.fixture.example:8443",
    "redirectUri": "https://callback.fixture.example:9443/integrations/mcp/callback",
    "authorities": [
      "mcp.fixture.example:8443",
      "issuer.fixture.example:8443",
      "authorization.fixture.example:8443",
      "token.fixture.example:8443"
    ]
  },
  "tls": {
    "certificateFile": "/absolute/owned-run/server.pem",
    "keyFile": "/absolute/owned-run/server.key",
    "caFile": "/absolute/owned-run/ca.pem"
  },
  "control": {
    "host": "127.0.0.1",
    "port": 9444,
    "origin": "https://localhost:9444",
    "tokenFile": "/absolute/owned-run/observer.token"
  },
  "protocol": "2026-07-28",
  "case": {
    "reply": "json",
    "response": "normal",
    "holdCalls": false,
    "requireAuth": true,
    "revision": 1
  },
  "apiKeyFile": "/absolute/owned-run/resource.key"
}
```

Origins must be canonical HTTPS DNS origins without a trailing slash, path,
query, fragment, credentials, or IP literal. The four data origins use the exact
listener port; distinct DNS names can reach that one listener. `authorities`
contains exactly their distinct canonical authorities (host and non-default port),
with no extra or duplicate
entry. The server checks both the request authority and the route's origin.
`/mcp` and protected-resource metadata belong to `resource`, authorization-server
metadata belongs to `issuer`, `/authorize` belongs to `authorization`, and
`/token` belongs to `token`. The resource identifier is `<resource>/mcp`.
`redirectUri` is the exact HTTPS redirect URL, with a DNS hostname and no
credentials, query, or fragment. It can have a separate authority, port, and
path. Authorization and token exchange require exact equality with this value.
It is not a fixture listener authority. The fixture does not follow the redirect.
Old shared-fixture tests retain their default `<origin>/callback` URL.

Supply a PEM leaf certificate signed directly by the supplied PEM CA, plus its
matching key. The leaf SANs must cover all data DNS names and the control DNS
name. The listener uses the supplied identity. Client code must trust that CA
and use ordinary DNS and HTTPS. No fixture fetch adapter or resolver is part of
the standalone server or the future acceptance transport.

`protocol` accepts `2026-07-28`, `2025-11-25`, or `2025-06-18` and stays fixed for
the process lifetime. Legacy sessions retain credential binding and explicit
cancellation. The paginated catalog contains `read_counter` and `write_counter`.
The schema and annotation fixtures stay in the shared server. `reply` accepts
`json` or `sse`. `response` accepts `normal`, `malformed`, `oversize`, `binary`,
`image`, `audio`, `resource`, `resource_link`, or `error`. These modes preserve the
write effect. `revision` accepts integers 1 through 100 and changes the catalog
schema and description. The remaining case keys are booleans. Defaults match
the existing fixture: modern protocol, JSON, normal results, no held calls, no
auth requirement, revision 1. Acceptance launchers must set `requireAuth: true`.

The resource key file contains a neutral 16–128 character ASCII token. The
observer file contains a different neutral 32–128 character ASCII token. Both
use letters, digits, `_`, or `-`, with optional surrounding whitespace. The
resource accepts `Authorization: Bearer <resource-key>` or its current OAuth
access token. The observer accepts only its own bearer token. Do not use a real
credential in either file or pass credential values in command arguments.

## Fixed control and readiness contract

All control routes use the separate HTTPS control listener and exact configured
control authority. All require the observer bearer token. Wrong authentication
returns 401, wrong authority returns 421, unknown routes or methods return 404,
and invalid input returns 400. POST requests use JSON and reject unknown keys.
Bodies are limited to 4 KiB. Responses have `Cache-Control: no-store`.

| Method and route    | Request                                      | Response or effect                                                                              |
| ------------------- | -------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| GET `/v1/ready`     | No body                                      | `version:1`, `ready`, resource URL, issuer origin                                               |
| GET `/v1/observe`   | No body                                      | `version:1`, durability health, effect state, read/held/cancellation counts, OAuth observations |
| POST `/v1/case`     | Any subset of the case keys above            | Applies checked controls; returns 409 while any call is held                                    |
| POST `/v1/release`  | `{}`                                         | Sends all held responses; future calls still use the selected hold mode                         |
| POST `/v1/cancel`   | `{}`                                         | Closes all held responses; each close increments cancellation count                             |
| POST `/v1/oauth`    | Optional `account`, `scope`, and/or `issuer` | Sets neutral account and scope returned on the next token issue or refresh                      |
| POST `/v1/shutdown` | `{}`                                         | Replies, then closes both listeners and all held connections                                    |

OAuth accounts are `account-one` or `account-two`. Issuer mode is `configured`
(the configured issuer) or `mismatch` (the existing neutral wrong-issuer case,
`https://other.example`). This changes only the callback `iss` field. Scope controls are
`tools:read`, `tools:write`, or `tools:read tools:write`. The scope control can
produce an account/scope change for the consumer to detect. It does not confer
production authority. Observations contain only the selected account, scope
override (or null), and counts of authorizations, code exchanges, rotations,
and rejected token/authorization requests. Issued codes/tokens, client secrets,
headers, arguments, and results are absent. OAuth grants are process-local; only
external write effects persist across fixture restarts.

Existing public/confidential clients, PKCE S256 checks, exact resource and
redirect checks, single-use state/code checks, rotating refresh tokens, and
scope-subset checks are reused. Public client ID is `fixture-public`;
confidential client ID and neutral secret retain the shared fixture values
(`fixture-confidential`, `fixture-client-secret`). The fixed fixture does not
support dynamic clients or arbitrary proxy destinations.

Startup emits one JSON line with `version:1`, `kind:"ready"`, control origin,
and resource URL only after durable state and both listeners are ready. A
startup failure emits only `{"version":1,"kind":"startup_failed"}` and exits
with code 1. No secret, arguments, raw results, or raw error is printed. The
launcher must also call authenticated `/v1/ready`; an open TCP port alone is not
readiness. `ready` becomes false if a durable write fails. SIGTERM and SIGINT
close both listeners and held connections and wait for closure. The launcher
must retain exact process handles, enforce its deadline, then stop/kill/wait on
all paths. It must not restart the fixture when it restarts the worker or
orchestrator for acceptance.

Bounds: 64 held calls, 128 legacy sessions, 128 observed protocol requests,
64 data connections, 32 control connections, 64 KiB protocol request bodies,
5 second request/header timeouts, and 1 second idle keepalive. OAuth retains at
most 128 pending code/refresh grants and 1,024 used states per process. Reaching
a bound fails the operation; it never skips an executed write.

## Durable effect evidence

`effects.json` holds `{state,sha256}`. `state` is version 1, with `executions`,
`count`, and the last 64 events. Each event has only a monotonic `sequence` and
SHA-256 `callHash` of the neutral transport-session/request-ID tuple. It stores
no raw ID, delta, argument, result, or credential. `executions` counts every
accepted write, including repeated request IDs; `count` is the counter's current
value. Reads and malformed requests do not increment it. Read/cancellation/OAuth
counts in the observer are process-local.

Each write makes a bounded replacement record in exclusive `effects.next`,
fsyncs that file, renames it to `effects.json`, then fsyncs the directory. Only
then can the server send or hold its reply. A committed held write is observable
through `effects.executions` and `held:1` before its client receives a response.
The observer does not acknowledge writes independently of this barrier.
Duplicate IDs are never suppressed. A reopened process loads the committed
record and continues both counters. Truncated, malformed, oversized, wrong-hash,
invalid-sequence state, or an incomplete `effects.next` fails startup. No reset
is automatic. A durable commit error blocks later commits and marks observer
health false. A failed record requires explicit launcher diagnosis and a new
run directory; do not delete evidence and continue the same acceptance run.

## Focused local proof

From the repository root, with Node 24 and existing worker dependencies:

```sh
mkdir -p var/mcp-stack-fixture
node --test --test-concurrency=1 --test-timeout=30000 \
  mcp-worker/test/fixture.test.ts \
  tests/fixtures/mcp-stack/durable.test.mts \
  tests/fixtures/mcp-stack/process.test.mts
node mcp-worker/node_modules/typescript/bin/tsc \
  -p tests/fixtures/mcp-stack/tsconfig.json --noEmit
```

The tests use private real directories below `var/mcp-stack-fixture`, supplied
neutral certificates, one standalone process at a time, only PATH/TMPDIR in its
environment, 29 second process watchdogs, exact stop/kill/wait cleanup, and no
provider. A separate-origin config test uses the local OpenSSL CLI to supply
neutral DNS SANs; it does not create DNS entries or a non-loopback listener.
The SDK smoke uses native fetch with explicit test CA trust. The wire tests use
Node HTTPS with that CA and normal localhost resolution. Tests include a
committed barrier, SIGKILL/reopen persistence, repeated-ID increments, API-key
and OAuth flow, observer authentication, invalid config/state, cancellation,
and graceful shutdown while held.

This is fixture unit/process proof. It is not worker routing, guest isolation,
CNI, Firecracker, VZ, provider, or full-stack proof. The integration owner must
invoke this focused command in the applicable CI lane and supply real network
and process supervision for physical acceptance. Production enable defaults
are unchanged.
