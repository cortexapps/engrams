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
then saves that config ID to the owned Docker archive. Before conversion,
the saved raw config must hash to the build config ID. After conversion to
OCI, the full config must match with JSON object keys sorted; value types
and list order must stay the same. The final OCI image ID and manifest digest
come from its actual raw bytes through the same strict archive and import
checks as other images.
Missing or invalid metadata or image facts stop the producer before save.
The local-directory export path stays unchanged. No tag or new builder is used.
Do not start Docker Desktop, Lima, Colima or another engine from the job.

For FC reserve the entire dedicated Linux x86-64 kernel. Prepare a fresh,
exclusive dm-thin pool with the production node helper before running the job:

```sh
install -d -m 0700 /work/mcp-thin/acceptance
POOL_NAME=mcp-acceptance POOL_DIR=/work/mcp-thin/acceptance \
  POOL_DATA_SIZE=96G POOL_META_SIZE=2G POOL_RESERVE=16G \
  sh deploy/helm/engram-host-fleet/files/thin-pool.sh
```

This is explicit node preparation, never a preflight action. Use an empty,
disposable directory and no `POOL_CACHE_DIR`. A pool name alone does not isolate
DM control. No other host agent, Firecracker, proxy, or storage job may run.
Install `/usr/sbin/dmsetup`, `/usr/sbin/losetup` and `/usr/sbin/thin_dump`.
The adapter verifies full allocation and actual backing identities, active and
inactive IDs, capacity and holders before the production host can open the pool.
The 96 GiB allocation is an operator estimate. Capacity is checked against
actual installed image/session/swap overlap before infrastructure passes;
a larger image can require more capacity. Keep 16 GiB of filesystem space free
in addition to pool data and metadata. Source tests do not prove these minima
are sufficient for a particular installed image.

The trusted host alone receives SYS_ADMIN, NET_ADMIN, MKNOD, mapper control,
the admitted pool and read-only metadata devices, and an `rwm` device rule for
the kernel's observed DM major. Only its private node directory binds at
`/dev/engram`; there is no broad `/dev` mount. Other roles retain their existing
device denial and private PID namespaces. The real infrastructure stage must
create, use and release production dm-thin session volumes under this effective
policy before MCP cases start. Missing capability fails without a retry that
adds authority. A device rule cannot isolate global mapper control; kernel
exclusivity is required.

Cleanup uses normal session deletion while the host is alive, then verifies
closed FDs and removal of live/transient volumes. Recorded images/checkpoints
can remain, with exact table/ID/device and zero opens. Any unexplained survivor,
foreign change or inactive ID fails cleanup. No forced DM removal, pool reset,
cache clearing or history deletion is allowed. Retain restricted evidence for
identity-scoped recovery on a failed run.

Provision native private DNS and routes for the exact fixture hosts and the
selected bridge subnet. Native host processes must reach container IPs, and
containers must reach the native host address. Provide real KVM and TUN access
on Linux, and real hardware virtualization on native arm64 macOS. Production
Firecracker uses the production dm-thin rootfs path; the platform's reviewed native
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
  "thinPool": {
    "name": "mcp-acceptance",
    "directory": "/work/mcp-thin/acceptance"
  },
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

An administrator can set the optional repository variable `MCP_FC_BOOT_ARTIFACTS`
to an absolute path such as `/etc/engrams/mcp-boot/manifest.json` on the dedicated
FC runner. With this variable absent or empty, CI keeps the strict existing boot
artifact download path. A download failure does not select another source. With
an explicit value, CI passes it as one quoted `--boot-artifacts` argument. The
variable contains a path, never a command, credential, or supplied result.

Before the job, the operator must install that manifest and its four pinned
files in the same canonical private directory outside the checkout and exported
source. The directory must be owned by root and mode 0700. The manifest and each
file must be root-owned, single-link, mode 0400, and have no symlink components.
Do not generate this custody record in the workflow. The manifest pins the
Firecracker executable, kernel, companion kernel checksum, and build provenance
with exact `{ "path": "...", "sha256": "..." }` objects. Its fixed identity is
version 1, kind `mcp-physical-boot-assets`, backend `firecracker`, architecture
`amd64`, and kernel release `fc-kernel-6.1.102-1`. `forkGitlink` must match
`third_party/firecracker` in the checkout. The provenance must identify a release
build for `x86_64-unknown-linux-musl` from that exact fork revision and bind the
executable hash. The producer checks the actual static x86-64 executable, all
pins and the kernel checksum before the build. It stages verified copies in the
private run directory. It rejects unknown fields, substituted files and invalid
provenance. VZ does not accept this option.

The repository variable and preinstalled files are administrator-controlled
runner preparation. This source change does not set the variable or provision a
runner. The same serialized FC job, 24 common cases, cleanup checks and `CI Gate`
remain required with either explicit source selection.

VZ omits `thinPool`, sets backend`vz` and additionally supplies canonical executable paths for
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
