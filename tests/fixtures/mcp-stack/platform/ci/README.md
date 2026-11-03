# Required physical MCP CI

`mcp-physical-fc` and `mcp-physical-vz` are required through CI Gate. Each
job builds and consumes its own complete assets. Both use the same path
selector. There is no dependency on a path-skipped binary or image job.
A missing setting or capability fails the job. A source test or a skipped
hypervisor test cannot replace either physical result.

Provision dedicated, disposable, operator-controlled runners with Python3.12+
and these labels:

- `[self-hosted, Linux, X64, engram-mcp-firecracker]`
- `[self-hosted, macOS, ARM64, engram-mcp-vz]`

These labels are a prerequisite, not evidence of a registered runner. The
repository runner inventory had no registered runners when this source was
prepared. Privileged FC public-source jobs require an ephemeral, disposable dedicated
runner VM with no standing platform, cloud or user credentials. Do not run
untrusted jobs on a persistent machine with credentials.
The workflows do not register runners or change repository settings.

The Linux producer and adapter run as root. The VZ producer and adapter run
as the runner UID. Provision a canonical, already-running Linux Docker engine
of the matching architecture. Its Unix socket must belong to that UID, have
no symlink components, and stay at the same inode/device. Docker CLI/server
must satisfy composition preflight (at least26, API1.45). Install buildx as a
system CLI plugin; the producer uses the engine's default Docker driver with
explicit `--config`, `--host` and `--builder default`. It creates no extra
BuildKit daemon. Provision the classic image store with raw config SHA256
image IDs. A store that exposes manifest IDs fails the strict image checks.
The default driver must support image loading and the local exporter.
For source images, the producer uses `--load` and retains build metadata in
its private work root. It accepts only `containerimage.config.digest`, checks
the exact loaded image ID, Linux architecture, source revision and run label,
then saves that config ID to the owned Docker archive. It converts that
archive to OCI, checks its raw config against the build config ID, and runs
the same archive and import checks as other images.
Missing or invalid metadata or image facts stop the producer before save.
The local-directory export path stays unchanged. No tag or new builder is used.
Do not start Docker Desktop, Lima, Colima or another engine from the job.

For FC reserve the entire dedicated Linux kernel for this job. Preload the
production NBD module and its complete device set before CI. There must be no
other NBD users, host agents, Firecracker or proxy instances on that kernel.
The adapter must verify that the selected engine and native runner share the
same kernel/boot and initial PID namespace, discover nbds_max1..4096, and
qualify every existing slot as usable and free. It reserves the whole verified
set across restarts (FIRST0/MAXactualcount/WARM1). Capacity exhaustion fails;
File-memory restore does not remove the NBD rootfs requirement. The workflow
serializes FC jobs per repository; the operator must also reserve the entire
runner/kernel across other repositories, services and runner registrations.

Only the trusted FC host uses the host PID namespace for production NBD PID
semantics. Its mount, cgroup and guard network namespaces stay private, with
read-only root/proc/sys/sysfs/cgroup, default seccomp and only the exact required
capabilities/devices. It receives verified individual NBD block devices plus
KVM/TUN. It receives no broad /dev mount or SYS_MODULE. The worker and every
other container retain private PID and their existing authority. The trusted
host can observe kernel PIDs and has global NBD netlink authority; device
cgroups do not isolate that control plane. Exclusive kernel reservation is an
operator prerequisite, not a supplied PASS flag.

Normal cleanup destroys only the run's real sessions through supported APIs
while the host is alive. The adapter must observe VM exit and complete NBD
release, then stop the host and prove no owned host/FC/UFFD/proxy process
survives. A stale binding or unexpected host death is cleanup failure and needs
identity-scoped operator recovery. CI never forcibly disconnects NBD, resets
the module, or deletes authoritative history to manufacture cleanup.

Provision native private DNS and routes for the exact fixture hosts and the
selected bridge subnet. Native host processes must reach container IPs, and
containers must reach the native host address. Provide real KVM and TUN access
on Linux, and real hardware virtualization on native arm64 macOS. Production
Firecracker also uses the Linux NBD rootfs path; the platform's reviewed native
capability preflight must qualify that path before a VM starts. No workflow
changes modules, device permissions, sysctl, routes or global DNS. A failed
capability probe is a failure. The VZ host is built and ad-hoc signed with the
production virtualization entitlement before its file hash is calculated.

Place a bounded nonsecret JSON file at the fixed path:

- Linux: `/etc/engrams/mcp-physical-firecracker.json`
- macOS: `/Library/Application Support/engrams/mcp-physical-vz.json`

The file must be canonical, single-link, at most16KiB, owned by root or the
runner UID, and not writable by group/other. Exact schema (replace paths and
addresses with the provisioned facts):

```json
{
  "version": 1,
  "backend": "firecracker",
  "engineSocket": "/run/docker.sock",
  "nativeHostIp": "192.168.80.2",
  "subnet": "10.88.2.0/24",
  "registryPort": 30000,
  "buildRegistryPort": 31000,
  "tools": {
    "node": "/opt/mcp/node/bin/node",
    "bun": "/opt/mcp/bun/bin/bun",
    "docker": "/opt/mcp/docker/bin/docker",
    "openssl": "/opt/mcp/openssl/bin/openssl",
    "skopeo": "/opt/mcp/skopeo/bin/skopeo",
    "gh": "/opt/mcp/gh/bin/gh",
    "oras": "/opt/mcp/oras/bin/oras",
    "npm": "/opt/mcp/node/lib/node_modules/npm/bin/npm-cli.js"
  }
}
```

VZ sets backend`vz` and additionally supplies canonical executable paths for
`cargo`, `rustc`, `cc` and `go` under tools. Supply actual Rust1.99.0 toolchain
binaries, not rustup shims. Supply Go1.26.2, Node24 (CI source uses24.22.2),
Bun matching `.bun-version`, OpenSSL3 and the native build prerequisites,
including protoc and Xcode command-line tools. Runtime copies must remain
executable with their normal platform library dependencies. The VZ build uses
fresh private Cargo/Go caches, real production host/proxy source and the actual
netstack build script. It compiles the read-only native process identity probe.

The native address is a nonloopback RFC1918 address on a real noninternal
interface, separate from the nonoverlapping10.x.y.0/24 subnet. Reserve
registryPort through registryPort+3, native host9101 and the fixture service
ports checked by the adapter. buildRegistryPort is separate. On FC only it
publishes a run-owned registry at127.0.0.1. The default engine must support
its normal loopback registry access without a daemon setting change. The
selected engine's build resolver must reach that same loopback publication.
This registry publishes no external image. Do not add credentials, commands,
scripts, environment maps or PASS booleans to operator JSON.

Before any asset operation, the producer makes a fresh private checkout owned
by its executing UID and reexecutes the fixed producer there. This also puts the
actual adapter, its Git reads and private dependency installation under the same
principal. The bootstrap reads only the exact canonical original source with
command-local `safe.directory` trust, disables optional index writes, and fetches
the exact HEAD through local Git into a new repository with an empty template.
It verifies HEAD, clean files, every file owner and the small generated Git
config; source credentials, config, hooks and hard links are never copied. No
global trust entry or shared-checkout ownership change is used. The outer work
root retains bootstrap logs and safe summary; the private checkout is at
`checkout/` and actual assets/evidence/private caches are under `execution/`.
The inner stage accepts its private flag only with that owned canonical layout.

The producer verifies clean exact HEAD and exports only its tracked source
with git archive into a new private directory. It excludes only the known legacy
runtime directory `crates/engram-coordinator/var` before validation and archive
creation. All other secret/var/.git paths fail validation. The archive uses the
same finite list of literal tracked names. Before extraction, the producer checks
all archive member paths and types against that selection, including parent
directories and empty submodule directories. Missing, extra, duplicate, linked,
or unsafe members fail before any file is extracted. Safe tar extraction remains
enabled. It never sends the checkout, node_modules, runtime files, `.env` or Docker
configuration as a build context. Dependency installation uses private cache,
explicit empty npm configuration and XDG Bun configuration, with lifecycle
scripts disabled; no user package-manager config or inherited token is used. It uses actual production Dockerfiles for
coordinator/orchestrator/worker/demo, unchanged reviewed guard/fixture recipes,
and the fixed Linux-musl recipe here. That recipe builds real agentd and both
harnesses, the deterministic model CLI, and production bundle leaves for the
selected Linux architecture. guestInit is the exact DEFAULT_INIT_SHIM source
bytes. Only the model CLI is a replacement.

The fixed dependency runtime paths and explicit users match composition:

| Image    | Runtime user | Command and writable paths                                                                                                    |
| -------- | ------------ | ----------------------------------------------------------------------------------------------------------------------------- |
| Postgres | 999:999      | production entrypoint; `/usr/bin/pg_isready`; owned `/var/lib/postgresql/data` volume and `/var/run/postgresql`, `/tmp` tmpfs |
| Blob     | 1000:1000    | `/bin/fake-gcs-server`; owned `/data` volume, `/tmp` tmpfs                                                                    |
| Registry | 1000:1000    | image entrypoint; owned `/var/lib/registry` volume, `/tmp` tmpfs                                                              |
| DNS      | 1000:1000    | `/coredns`; read-only `/projection/Corefile` and hosts file                                                                   |
| Edge     | 1000:1000    | `/usr/sbin/nginx`; read-only `/projection`, writable `/tmp` tmpfs                                                             |

The producer-only loopback registry uses its default root user with a private
tmpfs store. These source path assumptions must pass the actual image and
physical consumer gates; no default image user substitutes for explicit
composition users.

`upstream-images.json` pins five upstream single-platform manifests for each
architecture. Anonymous registry metadata supplied those facts. Actual CI
checks the pinned upstream raw manifest, then converts with skopeo to an OCI
archive. OCI/Docker media type conversion can change the manifest digest; the
final Inputs use the actual OCI manifest and config, never the upstream tag or
a substituted digest. The archive file SHA, raw OCI manifest SHA and raw config
SHA are three separate facts. Skopeo converts the same OCI archive to a Docker
load archive; explicit engine inspect checks config ID/OS/architecture and
current source labels. The actual adapter independently checks all archive
linkage before real public image enable and rootfs materialization.

For FC the production host Dockerfile builds the base at current HEAD. A
loopback run-owned registry makes that immutable manifest available to the
reviewed host derivative. It adds only the gitlink-keyed production fork.
The final image supplies the exact host/proxy/UFFD/fork pin bytes through
`docker cp` from an unstarted run-owned extraction container. The staged fork
must match the baked file. There is no stock FC fallback or skip.

The workflow token is permitted only for fixed upstream fork/kernel reads.
The producer can run anonymously when those artifacts are public. Fixed `gh
release download` children receive only GH_TOKEN and a private GH_CONFIG_DIR.
The production kernel release checksum must match. Fixed GHCR oras login uses
password stdin and a private0600 registry config deleted after fetch. The
resolved immutable fork manifest and gitlink are retained. No credential reaches
builds, Inputs, the adapter, CLI arguments or public logs. Missing artifact read
permission remains a hard failure and a release prerequisite.

The producer invokes the actual strict Inputs writer with Node24 transform
types, then the exact FC/VZ wrapper with a private Inputs file and absent
canonical run root. The unchanged common matrix must produce exactly one
PASS result with selected24 and passed24. The actual adapter completion must
confirm the same backend/HEAD, matrix completion and no cleanup failures.
A zero-case run, missing output, failure, cancellation or cleanup failure fails.

Work roots under RUNNER_TEMP are0700 and retained on operator-controlled
storage. Logs, journal, evidence, database volumes and effect history can
contain sensitive test material. Limit access and retention there. Public
artifacts contain only the fixed safe status/count summary. Never upload raw
Inputs, context, environment, logs, HTTP bodies, TLS keys, cookies, tokens or
KEK. Producer cleanup removes only exact planned label-owned registry and
extraction containers and checks absence. It also requires the actual adapter
completion/cleanup files and queries that run's labels for container/network
survivors. The adapter owns native/kernel cleanup and its graph cleanup.

Each command starts a new process session. Its group stays in the ownership
map through success, nonzero exit,
timeout and cancellation. TERM, then KILL and bounded actual group-absence
checks drain descendants even after the leader exits or closes its streams.
Only ESRCH proves group absence; inaccessible groups remain unresolved. A group
that remains fails cleanup, with its ownership and private event log retained.
The adapter keeps its240s graceful cleanup allowance.

No global prune or foreign process/resource removal occurs. A missing engine
query or survivor is failure. A hard runner termination needs operator recovery
of its exact run-owned resources and private evidence; no source claim can
prove cleanup after loss of the machine.

Primary implementation references:

- [Docker buildx build](https://docs.docker.com/reference/cli/docker/buildx/build/)
- [Docker file export](https://docs.docker.com/build/building/export/)
- [Skopeo copy](https://github.com/containers/skopeo/blob/main/docs/skopeo-copy.1.md)
- [OCI destination media types](https://github.com/containers/image/blob/main/oci/layout/oci_dest.go)
- [Container transports](https://github.com/containers/image/blob/main/docs/containers-transports.5.md)
- [Bun global configuration](https://bun.com/docs/runtime/bunfig)
- [Bun npm configuration](https://bun.com/docs/pm/npmrc)

This source is runnable only after operator provisioning and reviewed
composition/Inputs integration. No local image build, engine call, container,
service, database, helper, VM, physical matrix or remote CI run is established
by the producer's source tests.
