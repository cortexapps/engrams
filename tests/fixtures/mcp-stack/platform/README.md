# Isolated platform matrix

The two adapters run the same common MCP matrix with real services and a real
Firecracker or VZ host. They create only resources for one fresh run. Source
checks do not establish either physical backend result.

```sh
bash .github/scripts/mcp-stack-fc.sh --run-root /private/absent-run --artifacts /private/fc-inputs.json
bash .github/scripts/mcp-stack-vz.sh --run-root /private/absent-run --artifacts /private/vz-inputs.json
```

The selected Bun must match the manifest's binary hash and `.bun-version`.
The wrappers select a local Bun, clear the inherited environment, and pass only
these arguments. `main.ts` qualifies the platform, creates the stack, enables
the guest image through the public API, writes the strict private context,
starts the authenticated supervisor, then invokes the unchanged command:

```sh
bun orchestrator/scripts/mcp-stack/run.ts --context /private/absent-run/context.json --matrix all
```

A missing prerequisite, invalid input, failed readiness check, failed matrix,
or cleanup failure returns nonzero. There is no successful skip or alternate
case list. Deployment defaults remain disabled.

## Operator inputs

`preflight.ts` defines the exact version 1 manifest schema. No command,
environment, credential, receipt, extra image, or extra role field is accepted.
Paths must be canonical, owned files with one link. The manifest is private.
The run root must not exist; its parent and the empty engine config directory
must be owned, canonical directories with mode 0700.

Required binary pins are `node`, `bun`, `docker`, `openssl`, `hostAgent`,
`kernel`, `helper`, `claudeHarness`, `codexHarness`, `modelCli`, `agentdBundle`,
`sentinelBundle`, `guestToolsBundle`, and `guestInit`. Each pin has `path` and
lowercase `sha256`. Node is 24; OpenSSL is 3. The native host, helper, and proxy
must have the selected native architecture. Harness and model binaries target
Linux with that architecture. Guest kernel checks are backend specific.

`proxy` is required for both backends. FC also requires `firecracker` and the
`hostAgent` image. VZ forbids both FC-only fields, requires arm64, and requires
`nativeProbe`, compiled from `native-process-identity.c` on macOS:

```sh
cc -std=c11 -Wall -Wextra -Werror -O2 tests/fixtures/mcp-stack/platform/native-process-identity.c -o /private/nativeProbe
```

Hash final binary bytes. Sign the VZ host with the virtualization entitlement
before hashing it. The probe reads kernel PID, UID, executable, parent, group,
and birth seconds plus microseconds. It accepts only the run's host, proxy,
and managed netstack paths. It has no signal or arbitrary process API.

Required image keys are `postgres`, `blob`, `registry`, `dns`, `edge`, `guard`,
`coordinator`, `orchestrator`, `worker`, `fixture`, and `guest`. Each image has
an `archive` file pin, `manifestDigest` for the raw single-platform OCI
manifest, and `id` for its config blob and local engine image ID. Every image
must already be loaded in the selected engine. Application images and the
FC host image must carry the exact `sourceCommit` revision label.

The input also selects the exact owned Unix engine socket, engine ID, empty
`engineConfig`, isolated private `/24`, nondefault registry port, and actual
`nativeHostIp` outside the fixture subnet. Docker CLI and server must be 26
or later and support API 1.45 or later. Every engine call passes explicit
`--config` and `--host` arguments, checks config/socket identity before and
after, and excludes parent credentials and default contexts.

The operator must supply actual private DNS and routes from the native host
to all exact fixture names, including `postgres.fixture.example`, and to the
Linux substrate. Native resolution is checked. TLS, database, fixture, host,
and matrix operations test their real routes. No adapter installs a VM
manager or changes host DNS, host routes, or global engine settings.

## Image and service contracts

The guard image must provide `/bin/sh`, `/bin/bash`, `/bin/sleep`, `/bin/mkdir`,
`/bin/chown`, `/bin/chmod`, `/usr/bin/find`, `/usr/bin/sha256sum`,
`/usr/bin/skopeo`, `/usr/sbin/nft`, `/usr/bin/dig`, `/usr/bin/nc`,
`/usr/bin/curl`, and `/usr/bin/mksquashfs`. Production bundle scripts also use
coreutils. `nc` must use the OpenBSD exit/timeout behavior; `dig` must use BIND
exit status 9 for unreachable DNS. Tool availability is checked before use.
Guard archive work runs with the caller's UID. Projection staging uses root
with only CHOWN, DAC_OVERRIDE, and FOWNER. Network guards use root with only
NET_ADMIN. All start with the default capabilities dropped.

The fixture image must provide Node 24 at `/usr/local/bin/node` and the real
fixture entrypoint at `/app/tests/fixtures/mcp-stack/main.mts`, with its source
imports. Modern and legacy fixtures are independent processes with generated
TLS and durable control/effect history. Their writable mounts contain only
their exact run roots. They stay alive across application restarts.

Postgres must provide UID 999, `/usr/bin/pg_isready`, and its normal initialized
image entrypoint with `PGDATA=/var/lib/postgresql/data`. This acceptance fixture
uses one fresh `mcp_<run_id_with_underscores>` database for both real production
migration paths. Coordinator and orchestrator have distinct private DSN files.
Their strict read-only observers check each role's real schema. No authority,
image, placement, OAuth, publication, or user row is inserted by the launcher.
SQL source has no shared table, index, or function name across the two migration
sets. Real migration failure remains a hard failure; source inspection cannot
prove migration compatibility.

Blob uses `/bin/fake-gcs-server` on HTTP 4443 with UID 1000 and `/data` storage.
Registry uses its inspected entrypoint, UID 1000, and `/var/lib/registry`.
CoreDNS uses `/coredns`, UID 1000, and the generated no-forward Corefile.
The TLS edge uses `/usr/sbin/nginx`, generated config, and writable `/tmp`.
Select upstream image bytes that meet these explicit user, path, environment,
and volume contracts. Automatic image volumes and unexpected image environment
keys fail the effective audit. Application entrypoints remain the production
image entrypoints. O migrations use its production `bun run migrate` leaf.

The FC-only `host-agent.Dockerfile` derives from a production host image at an
immutable manifest digest and adds only an independently hashed Firecracker
binary. Pass `HOST_IMAGE=registry/name@sha256:...` and `FIRECRACKER_SHA256=...`,
with `firecracker` in a private build context. It inherits the source revision
and entrypoint. Pin the derivative archive/manifest/config. Extract the final
host, UFFD, and proxy bytes for their binary pins; the launcher checks all four
baked executable hashes before releasing the host entrypoint.

FC runs in one owned private PID/mount namespace with KVM and TUN devices,
NET_ADMIN and SYS_ADMIN, bounded resources, and exact run-owned mounts. It
shares only C's guard network namespace. Its listener is `0.0.0.0:9101` inside
that namespace; C's guard publishes only `nativeHostIp:9101`. The native address
and port are checked first. The advertised TLS authority stays
`host.fixture.example:9101`. VZ binds its verified native address directly.
No host PID namespace, host network, engine socket, or host root is mounted.

## Network and startup order

One internal bridge carries isolated PG, blob, DNS, fixtures, and guards. O,
C, and worker join persistent guard namespaces with exact default-drop nft
rules. The owned registry shares C's guard and is published only on
`127.0.0.1:<selected-port>` for asset bootstrap. Managed MCP destinations
retain strict HTTPS and nonloopback checks.

A named volume contains only the fixed `resolv.conf`, with file mode 0644 and
directory mode 0755. A read-only `volume-subpath=resolv.conf,volume-nocopy`
mount supplies each container's actual `/etc/resolv.conf`. No host path is
exposed to the worker. Its other mount is only its exact config/TLS projection;
private bounded tmpfs supplies temporary writes. Secret configuration volumes
retain directories 0700 and files 0600. Only the resolver and guard CA-only
probe projection are public within their isolated volumes.

The old copy-to-intrinsic-file approach is not used: Docker archive extraction
rejects writes through a read-only root filesystem. Moby's
[file subpath implementation](https://github.com/moby/moby/blob/master/daemon/volume/safepath/join_linux.go)
and [mount setup](https://github.com/moby/moby/blob/master/daemon/volume/mounts/mounts.go)
provide the named file projection. See also [Docker copy semantics](https://docs.docker.com/reference/cli/docker/container/cp/)
and [volume subpaths](https://docs.docker.com/engine/storage/volumes/).

Before app release, the launcher audits effective mounts, exact subpath,
volume ownership, read-only state, namespace/user, capabilities/devices,
resources, environment, DNS options, publications, and inspected entrypoint.
It reads back actual resolver bytes. A fixed waiting shell then execs the
original entrypoint and arguments. No application code runs before these
checks. The FC host shell also sends the actual host output to its private
0600 observation log.

Qualification inspects installed nft JSON, resolves and reaches both real
HTTPS fixtures, confirms an owned denial sink is live, and verifies that
sink traffic, the embedded DNS stub, and a separate owned DNS stub are blocked.
Unknown DNS names receive refusal with no forwarding. Checks repeat after
start, status, and restart. A boolean or operator receipt cannot replace them.

Startup order is dependencies, both fixtures, guards and controls, TLS edges
and registry, worker readiness, coordinator readiness and real migrations,
O's real migration leaf, O readiness, native host readiness, normal bootstrap
human signup, public image enable, actual host image readiness, strict context,
supervisor, then the full common matrix. Only the deterministic model CLI is
replaced in the test harness bundles. Actual harness binaries, descriptors,
agentd, and the exact committed production init shim remain intact.

## Cleanup and evidence

The supervisor permits only its fixed six-role status/restart/stop protocol.
O disable variants change only O's enable switch; C, worker, host, operator
candidate, PG, blob, fixtures, and namespace identities remain normal.

Signals and deadlines stop and drain the common runner and supervisor, then
the native host and exact owned Linux resources. Engine CLI cancellation
uses the retained process group's TERM/KILL/wait path. Cleanup also recovers
exact planned container names if a daemon create succeeded but its response
was lost. It refuses foreign labels and records survivors as failure.

FC's private PID namespace bounds VM/proxy/helper lifetime; the container must
be stopped with PID zero before removal. VZ rechecks stable kernel birth,
UID, executable, and PID before signaling exact owned helper/proxy survivors,
including separate groups, then requires none remain. It sends no pattern kill.
Logs, native identity snapshots, matrix output, and cleanup/completion JSON
stay private. Named volumes retain inactive database and effect history; the
cleanup receipt names them. No guest/VM result is written by the launcher.

## Source checks and remaining physical gates

```sh
MCP_PLATFORM_TEST_OPENSSL=/absolute/openssl3 bun test tests/fixtures/mcp-stack/platform/platform.test.ts tests/fixtures/mcp-stack/platform/composition.test.ts
node orchestrator/node_modules/typescript/bin/tsc -p tests/fixtures/mcp-stack/platform/tsconfig.json --noEmit
bash -n .github/scripts/mcp-stack-fc.sh .github/scripts/mcp-stack-vz.sh
```

These tests use files, controlled subprocesses, synthetic engine replies,
private TLS, and private Unix sockets. They establish argument, identity,
authority, resolver, launch-order, cancellation, and cleanup rejection behavior.
Native C compile/own-process checks establish only local process identity.
No source test proves a real mount, installed policy, image materialization,
service/VM cleanup, public image enable, or matrix success. Both real backend
runs, their assets and capable runners, and current-head CI are still required.
