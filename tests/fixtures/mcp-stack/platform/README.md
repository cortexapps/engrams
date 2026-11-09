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
starts the authenticated supervisor, validates the complete context against that
real private socket, then invokes the unchanged command:

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
Linux substrate. For Firecracker, `host.fixture.example` must resolve to the
coordinator bridge address (`<subnet-prefix>.21`). For VZ, it must resolve to
`nativeHostIp`. Native resolution is checked against the same exact map used
by fixture DNS. TLS, database, fixture, host,
and matrix operations test their real routes. No adapter installs a VM
manager or changes host DNS, host routes, or global engine settings.

## Image and service contracts

The guard image must provide `/bin/sh`, `/bin/bash`, `/bin/sleep`, `/bin/mkdir`,
`/bin/chown`, `/bin/chmod`, `/usr/bin/find`, `/usr/bin/sha256sum`,
`/usr/bin/skopeo`, `/usr/sbin/nft`, `/usr/bin/dig`, `/usr/bin/nc`,
`/usr/bin/curl`, `/usr/bin/mksquashfs`, `/usr/sbin/ip`, and
`/usr/bin/openssl` (OpenSSL 3). Production bundle scripts also use
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
Only the fixed `dns` and `denied-dns` fixture roles add NET_BIND_SERVICE after
all default capabilities are dropped. The pinned executable has a file
capability that requires this capability in its bounding set, even when port
53 is unprivileged. Each effective audit requires exactly this one added
capability for these two roles and rejects it for every other role. Both DNS
roles retain read-only roots, no-new-privileges, default seccomp, and private
config projections. Worker and application capabilities do not change.
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

FC requires one exclusive disposable Linux x86-64/KVM runner. No other host
agent, Firecracker, proxy, or storage job can share its kernel. The operator
must prepare a fresh dedicated dm-thin pool before the job. The strict FC-only
`thinPool` input has exactly `name` and `directory`. Names match
`[A-Za-z][A-Za-z0-9_.-]{0,126}`; physical admission also requires at most 26
characters so production checkpoint names fit the kernel's 127-byte limit.
The directory is canonical, owned by the job UID, mode 0700, and has no symlink
components. VZ rejects this field.

Admission checks the real pool table/device, metadata/data loop devices and
backing file inodes/devices, full file allocation, holders and open FDs. It
checks all active and inactive thin IDs before the host can open the pool.
Production host startup deletes inactive orphan IDs; it must never open an
unqualified shared pool. Metadata inventory runs only after kernel process and
device-FD checks prove
that the host and its VMs have stopped. An offline lease blocks every owned
host start until the metadata snapshot is released. A pre-existing reservation
is refused, and an uncertain release stops further inventory. Live observation
and live release checks read tables and FDs without reserving metadata. No job
loads modules, creates or formats a
pool, changes node modes, clears caches, or force-removes devices.

Only the trusted FC host joins the initial host PID namespace. It retains a
private cgroup and mount namespace, C's owned network namespace, default
seccomp, read-only root/proc-sys/sysfs/cgroup mounts, and bounded resources.
It has NET_ADMIN, SYS_ADMIN and MKNOD. Exact KVM, TUN, mapper control, pool,
and read-only metadata nodes are projected. A block-device cgroup rule grants
`rwm` only for the running kernel's DM major; `/dev/engram` binds one private
run-owned node directory. This is global DM control on the exclusive runner.
All other roles, including the worker, retain private PID namespaces and no
DM authority. No broad `/dev`, host root, engine socket, or host network mount
is allowed. Effective container inspection and the real infrastructure smoke
must prove this contract; source tests do not establish installed capability.

`ENGRAM_THIN_POOL` selects the qualified pool. Admission requires healthy data/metadata fractions, metadata capacity from
the installed data-block count (48 bytes per block plus 25% reserve), fully
allocated backing files, and a separate
16 GiB filesystem reserve. Actual image/root/swap sizes are then checked against
two live roots, two checkpoints, two in-flight roots, corresponding swap overlap,
one staged image, and 25% data reserve before infrastructure can pass. This is
a serial-suite budget, not a volume count based on the 24 cases. A larger sparse
file cannot satisfy it. The production File memory restore mode stays unchanged;
UFFD helper bytes remain pinned without a claim of UFFD runtime qualification.

FC's listener is `0.0.0.0:9101` only inside its owned namespace. C's guard
publishes only the verified `nativeHostIp:9101`. The advertised TLS authority
stays `host.fixture.example:9101` and resolves to C's bridge address because
FC and C share that namespace. The published native port remains exact.
VZ resolves this name to and binds its verified native address directly.

## Network and startup order

One internal bridge carries isolated PG, blob, DNS, fixtures, and guards. O,
C, and worker join persistent guard namespaces with exact default-drop nft
rules. The owned registry shares C's guard and is published only on
`127.0.0.1:<selected-port>` for asset bootstrap. Managed MCP destinations
retain strict HTTPS and nonloopback checks.

C alone has an exact `/32` route to the native host. FC uses the internal
bridge gateway for this native route. C's host RPC uses its own bridge address,
with the same host TLS name and leaf pin. VZ has one additional
run-owned noninternal private IPv4 bridge attached only to C's guard. The
launcher checks its actual IPAM for overlap with fixture and native host
addresses, derives the exact interface from its endpoint IP and MAC, removes
that link's automatic default under the installed nft default-drop policy,
and installs only the native-host `/32`. Docker's internal bridge forwarding
rules block destinations outside its subnet, so the internal bridge alone
cannot reach the native Mac. No global route or firewall is changed. Both
networks have exact ownership and cleanup checks.

After actual host readiness, a fixed short-lived probe in C's namespace uses
only a private read-only CA/client certificate/key projection, resolver, and
bounded tmpfs. OpenSSL verifies TLS 1.3, SNI, hostname, and the actual first host
leaf fingerprint. Only a sanitized certificate digest is retained. Exact
probe removal, route/no-default checks, network membership, nft policy, and
native mTLS qualification repeat for status and restart.

Only the FC C guard has `net.ipv4.ip_forward=1` and
`net.ipv4.conf.all.route_localnet=1`; other roles have no configured sysctls.
The actual FC host and guard must share a network namespace and read both
values as 1. Qualification checks the actual production INPUT, FORWARD,
PREROUTING, and POSTROUTING rules and their order after readiness, status,
and restart. Read-only proc/sys remains in place.

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

Signals and deadlines stop and drain the common runner and supervisor. Final
teardown uses the authenticated public SessionService DeleteSession path for
bounded, verified run-owned sessions while the host is alive. It retains task,
session, operation, journal, capture, and effect history. An API return does
not prove teardown: the launcher verifies that live and transient thin volumes
are removed by the production delete path, VM/device FDs are closed, and no
live/transient mappings or device holders remain before stopping the host.
After reaping the owned processes, it also checks metadata for unknown thin IDs.
Recorded image and
checkpoint volumes can remain with their exact device, table and thin ID and
zero opens. Unknown, busy or changed state fails cleanup. No forced DM remove,
pool erasure, forced adoption, or history deletion manufactures success.

Host restart retains the admitted pool/backing identity and append-only physical
boot evidence. It inventories inactive IDs before allowing the replacement host
to open the pool. Each physical boot binds its rootfs symlink, exact session
volume, thin table/ID and pool, canonical device number, real Firecracker FD,
process birth/hash/cgroup, and an increase in kernel read sectors sampled during
that boot. Prior-boot counters cannot qualify a new boot. Missing or failed
observation writes fail acceptance.

The common infrastructure stage performs real guest create/use/delete before
MCP cases. Its fixed authenticated `infrastructure-ready` checkpoint requires
physical boot observations and normal thin release. It then stops the owned
host, verifies process and FD quiescence, inspects metadata, and restarts the host
under the same lifecycle lock. The returned status must show a new host boot and
unchanged identities for every other role. The worker cannot supply these facts. The same path runs in the full matrix; no second runner framework
or mocked PASS replaces it. Final cleanup checks retained processes and pool
state again after owned processes stop.

Engine CLI cancellation uses the retained process group's TERM/KILL/wait path.
Cleanup recovers exact planned container and network names after a lost create
response, refuses foreign labels, and records survivors as failure. VZ rechecks
stable kernel birth, UID, executable, and PID before signaling exact owned
helper/proxy survivors, including separate groups, then requires none remain.
It sends no pattern kill. Logs, identity snapshots, matrix output, and
cleanup/completion JSON stay private. Named volumes retain inactive database
and effect history; the cleanup receipt names them. No guest result is written
by the launcher.

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
