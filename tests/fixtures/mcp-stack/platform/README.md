# Platform substrate source

This directory contains a source library for an isolated MCP stack. Runnable
Firecracker and VZ adapters are pending. There is no entrypoint, shell wrapper,
context output, platform PASS, or full-stack test in this change.

## Source boundaries

- `owned.ts` checks private paths and pinned bytes, creates exclusive files,
  constructs a minimal child environment, and controls a retained process group.
  The only optional child environment key is `DOCKER_HOST`, with a canonical
  local Unix endpoint. Command output and time are bounded; errors contain no
  child output. These helpers do not track a VM or descendants that create a
  separate process group.
- `preflight.ts` parses fixed artifact inputs and provides read-only prerequisite
  checks. It checks source identity, file hashes, executable architecture, an
  explicitly selected Linux engine, local image identity, and host capability
  inputs. Docker inspection requires an owned, empty 0700 config directory, checks its
  identity before and after each command, and passes explicit `--config` and
  `--host unix://...` arguments with a cleared environment. It does not read the
  user's CLI configuration. It does not create an engine or modify one. A returned input object is
  not runtime qualification: KVM API/NBD operation, actual routes, image archive
  correspondence, and boot behavior still require the adapter.
- `identity.ts` uses a pinned OpenSSL 3 executable to create a private CA, eight
  distinct leaf identities, neutral secrets, and separate bootstrap/admin/owner/
  other passwords. It checks certificate signatures, DNS names, key pairs, and
  derives both pin formats from actual certificates. Output files are exclusive
  and private without relying on the caller's umask.
- `configuration.ts` uses the production zone, egress, runtime, and operator
  identity validators. It emits normal and zone-disabled O runtime files, one
  worker config, one coordinator config, host TLS configs, and one normal operator
  candidate. The local operator parser call checks serialization only; it does
  not publish or authorize a candidate. Global disable belongs in the future O
  launch environment: change only `MCP_ENABLED` to false and retain its runtime
  file. Zone disable changes only `workers[].enabled`. C, worker, host, and the
  operator candidate retain their normal bytes during both kill cases.
- `network.ts` produces the fixed fixture DNS records and default-drop nftables
  plan for O, coordinator, and worker namespaces. It compares an observed JSON
  ruleset with that exact plan, ignoring only handles and metadata. The parser
  cannot establish that rules are installed, that all traffic traverses them, or
  that DNS has no alternate path. Runtime probes remain mandatory.
- `supervisor.ts` implements authenticated Unix HTTP controls with the shared
  six-role schema. Requests can name only status, stop, or a declared restart
  variant. Fixture roles cannot restart. It bounds headers, bodies, responses,
  connections, time, and the control queue. It verifies status identity and
  restart scope. A failed lifecycle closes admission to later controls.

The supervisor's lifecycle implementation is still an adapter prerequisite.
It must derive status from actual owned resources and effective enforcement,
check cancellation before every mutation, and drain resource work before its
promise settles. Closing the supervisor aborts active work and waits under a
bound. An uncooperative lifecycle produces a cleanup failure. A replaced socket
path produces a cleanup failure before listener close can remove that path;
the retained listener is unreferenced and must be released by process exit.
Neither failure is successful cleanup.

## Explicit inputs and generated resources

The private input manifest has exact keys: version 1, backend, architecture,
source commit, engine Unix socket and identity, private empty `engineConfig`
directory, registry port, isolated subnet,
native host address, binaries, and images. VZ requires arm64. Firecracker also
requires pinned `firecracker` and `proxy` inputs. Every file pin has an absolute
canonical path and SHA-256 hash. Every image has a pinned archive plus a local
`sha256:` image ID. These two pins must be linked to an immutable OCI manifest by
the future asset bootstrap; independently checking them is insufficient.

Fixed binary keys are `node`, `bun`, `docker`, `openssl`, `hostAgent`, `kernel`,
`helper`, `claudeHarness`, `codexHarness`, `modelCli`, `agentdBundle`,
`sentinelBundle`, `guestToolsBundle`, and `guestInit`. Fixed image keys are
`postgres`, `blob`, `registry`, `dns`, `edge`, `guard`, `coordinator`,
`orchestrator`, `worker`, `fixture`, and `guest`. The manifest accepts no command,
environment, credential, success receipt, or role extension.

The operator must provide an existing Docker-compatible Linux engine, pinned
local tools and assets, suitable native host privileges, and actual private
fixture DNS/routes on the native host and the Linux substrate. VZ additionally
requires the entitled native host binary and real managed-netstack helper.
No VM manager is installed. No global engine setting, host DNS setting, shared
`.env`, development database, or shared bundle directory is modified.

The future adapter must create only per-run containers, networks, volumes,
private configuration projections, and evidence. The planned asset registry
uses an explicitly selected nondefault port published only on `127.0.0.1`, in
the coordinator guard namespace. It must refuse an occupied or foreign port.
This local asset transport is separate from managed MCP destinations, which
retain strict HTTPS and non-loopback transport.

A fixture-only DNS server must have no forwarding or fallback. Docker custom
networks use an embedded resolver, so `--dns` alone is insufficient. The adapter
must project and verify the actual resolver path, deny alternate DNS, and test
positive fixture resolution plus negative external resolution before any app
starts. See the [Docker network documentation](https://docs.docker.com/engine/network/).

## Remaining composition requirements

Four runtime risks remain open: exact resolver projection; role environment and
mount audits; archive-to-image/rootfs identity; and native host, helper, VM, and
separate process-group cleanup. The library does not claim to resolve them.

The adapter must also launch isolated dependencies, run normal migrations,
verify real service readiness, sign up a separate neutral bootstrap human,
enable the exact owned image through the public flow, observe real host assets,
construct the strict common context, and invoke the unchanged common matrix.
The common three human identities remain separate from the bootstrap actor.
Missing prerequisites must fail explicitly. No supplied status object may serve
as physical evidence. Production startup compatibility must be checked against
the assembled source before the full run.

## Focused verification

Use existing Bun, Node 24, dependencies, and an OpenSSL 3 executable. The test
suite creates only private files, controlled child processes, and a private
Unix test server. It starts no container, database, host agent, helper, or VM.
From the repository root:

```sh
MCP_PLATFORM_TEST_OPENSSL=/absolute/path/to/openssl-3 bun test tests/fixtures/mcp-stack/platform/platform.test.ts
node orchestrator/node_modules/typescript/bin/tsc --project tests/fixtures/mcp-stack/platform/tsconfig.json --noEmit
node mcp-worker/node_modules/prettier/bin/prettier.cjs --check 'tests/fixtures/mcp-stack/platform/*'
```

The test cases check private input rejection, architecture bytes, environment
exclusion, process-group stop, rule comparison, actual certificate generation,
production config validation, and supervisor security and cleanup. Test
lifecycle status is synthetic and establishes no runtime enforcement or VM
proof. Full-stack and platform acceptance remain separate required gates.
