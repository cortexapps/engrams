# Engram

Self-hosted, open-source orchestrator for ephemeral AI agent sandboxes.

Engram orchestrates [Firecracker](https://github.com/firecracker-microvm/firecracker) microVMs on Linux production hosts and adds the layer above them: warm pools, snapshot lifecycle (UFFD-backed restore), multi-host scheduling, and a pluggable cloud abstraction. A subprocess-based dev backend lets the entire orchestration layer run on macOS during development; an Apple Silicon backend (Apple Virtualization.framework) gives Mac devs real microVM isolation locally. Production isolation is always Firecracker.

It brings the Modal/E2B/Ramp-Inspect "ephemeral sandbox per task" pattern to open source so any organization can run their own without vendor lock-in. See `DESIGN.md` for the full architecture and roadmap.

## Status

**Phase 1 — done.** Orchestration layer end-to-end on the dev backend. Coordinator HTTP API, scheduler, warm pool, persistent session-event log with SSE replay (`Last-Event-ID` / `?since=N`), bearer-token auth, image baker (Dockerfile + `engram.toml` → rootfs), pluggable secret store (env / dotenv / GCP-stub), Postgres metadata store. End-to-end create → exec → delete on macOS via `engram-sandbox-process`. CLI covers session list/get/delete/logs and image build.

**Phase 2 — done.** `engram-sandbox-firecracker` drives real Firecracker microVMs end-to-end: typed HTTP client over the FC unix socket; `create`/`destroy`/`list`/`snapshot`/`restore`/`exec_stream` all wired and exercised by integration tests on a Linux dev VM. `restore` supports both `File` mode (synchronous read of `memory.bin`) and `Uffd` mode (lazy paging via `engram-uffd-handler` — sub-100ms resume). `exec_stream` reaches an in-guest `engram-agentd` over Firecracker's vsock proxy; the image baker injects a static-musl agent + init shim into ext4 rootfs images.

**Phase 3 — done.** Multi-host coordinator: host-agents run as a separate binary that dials the coordinator over WebSocket (bincode-over-WS with hand-rolled `request_id` demuxer + W3C trace context per RPC; `--mode=all` keeps single-binary `just dev` working by registering the local backend in-process). The warm `Pool` is host-side (`engram-host-agent::pooled_backend::PooledBackend`), so heartbeats carry real `(ready, target)` per `image_version` and `HostRegistry`'s scheduler ranks by snapshot affinity → warm pool → capacity. `LISTEN/NOTIFY` on `session_events` + `host_dead` lets coordinator replicas re-broadcast events into local SSE subscribers and drop dead hosts in lockstep. The dead-host auto-detector (`crates/engram-coordinator/src/dead_host.rs`) handles `kill -9 host → mark Dead within 30s`, racing replicas via `pg_try_advisory_lock` so exactly one wins each eviction. `sessions.sandbox_id` persistence + `repopulate_routing` on coord startup means Active sessions survive a coordinator restart. `GET /api/hosts` + `engram host list/get/drain` CLI shipped. Production hardening (TLS, `HostStatus::Disconnected`, backpressure tuning) lands with Phase 6.

**Phase 4 — done.** Hot-suspend keeps Firecracker memory snapshots for *local* idle eviction (Track B); harness protocol (`engram-harness-{proto,noop,claude}`) + in-VM supervisor (`engram-bootstrap`) wired end-to-end; preemption best-effort drain (Track D). The Claude Code adapter (`engram-harness-claude`) shipped as the reference adapter and runs end-to-end against real Firecracker microVMs. *Originally framed as "git as the workspace durability primitive"; superseded by ADR 0005 — see Phase 6 below.*

**Phase 4.5 — done (April 2026). Apple Silicon backend.** `engram-sandbox-vz` drives Apple's Virtualization.framework directly from Rust via the `objc2-virtualization` bindings — no Swift driver. Mac contributors get real microVM isolation locally (not just subprocesses). Multi-port virtio-console replaces vsock for the host↔guest control plane (universal kernel support, no `CONFIG_VIRTIO_VSOCKETS=y` requirement) — `engram-transport` is a backend-agnostic trait abstraction over both. APFS `clonefile(2)`-based snapshots replace VZ's broken-upstream `saveMachineStateToURL`/`restoreMachineStateFromURL` pair (~50 ms even for 1.7 GB rootfs); the snapshot is the clone. Cold boot 562 ms, cold resume 746 ms, warm-pool checkout 30 ms — same warm-pool code path as FC. ADR 0003 captures the design.

**Phase 5 — done.** Registry-backed image + harness distribution (ADR 0004). Bake images and harness packs are pushed to a Docker registry as custom OCI artifacts; the coordinator becomes stateless w.r.t. image data, and host-agents pull on first use into a content-addressable cache. Registry credentials live in Postgres envelope-encrypted under a deployment KEK (`engram-crypto`); `RegistryAuthSpec` is variant-discriminated so static creds, GCP Workload Identity, and future cloud IAM kinds all dispatch through one resolver. `engram registry`/`engram harness` CLI surface; per-deployment local dev via `registry:2` in `docker-compose`.

**Phase 6 — pivot complete (May 2026). Hot+cold snapshot durability + cold-tier blob (ADR 0005).** Git is fully retired from the platform layer. Workspace contents now come exclusively from the bake image's `/workspace`; agents that want to push code do it themselves inside the sandbox using credentials mounted via the existing `[secrets.X]` machinery. Two snapshot tiers replace it: **hot** (FC `memory.bin` + state file on Linux, APFS rootfs clone on VZ — local NVMe, sub-second resume on the same host) and **cold** (tar+zstd of the FC snapshot dir, uploaded via a `BlobStorage` backend — KEK-sealed blob URLs in Postgres). `POST /api/admin/sessions/:id/flush` and `POST /api/admin/flush-idle` expose the flush primitive explicitly; a host-agent disk-pressure detector calls the same `flush_session` primitive on the implicit trigger. `POST /sessions/:id/resume` dispatches three branches: hot (local NVMe), cold (download + untar + restore on any host with capacity), or 410 Gone if both tiers are gone. ADR 0005 captures the pivot end-to-end; ADRs 0001 + 0002 are superseded/amended.

**494 tests pass** via `cargo nextest run --workspace` (4 ignored: 2 live-Postgres integration tests, 1 entitlement-gated VZ smoke test, 1 live-cloud GCS round-trip), plus 5 Firecracker integration tests on the Linux dev VM and 14 VZ unit tests on macOS Apple Silicon.

## Workspace

```
crates/
  engram-core                       # types, traits, errors. No I/O.
  engram-protocol                   # wire types: bincode-over-WS Frame protocol (coordinator <-> host)
  engram-harness-proto              # wire types: harness ↔ host (HarnessEvent / HarnessCommand)
  engram-harness-noop               # first-party test harness (deterministic event cadence, no real agent)
  engram-harness-claude             # Claude Code adapter — reference agent harness implementation
  engram-transport                  # transport abstraction: vsock (FC) / virtio-console (VZ)
  engram-coordinator                # binary: HTTP API + scheduler + idle evictor
  engram-host-agent                 # binary: per-host daemon (warm pool, harness hub, flush primitive, disk-pressure detector)
  engram-image-builder              # binary: warm-image baker (Directory + Ext4 modes)
  engram-cli                        # binary: ops/admin tool
  engram-agentd                     # binary: in-guest exec daemon (transport-agnostic)
  engram-bootstrap                  # binary: in-guest supervisor (spawns the harness, handles re-attach)
  engram-sandbox-firecracker        # SandboxBackend: Firecracker microVMs (Linux production)
  engram-sandbox-vz                 # SandboxBackend: Apple Virtualization.framework (macOS Apple Silicon dev)
  engram-sandbox-process            # SandboxBackend: host subprocesses (DEV ONLY, no isolation)
  engram-uffd-handler               # binary: userfaultfd page-fault handler for fast FC snapshot restore
  engram-cloud-{gcp,static,mock}    # CloudBackend impls
  engram-secrets-{dev,gcp}          # SecretStore impls (env/dotenv; GCP Secret Manager)
  engram-postgres                   # MetadataStore impl
```

(ADR 0001 retired the `engram-storage-*` (`BlobStorage`) crates entirely; image distribution moves to a Docker registry in Phase 5.)

## Sandbox backends

The orchestration layer is VMM-agnostic — anything that implements `SandboxBackend` plugs in. We ship three:

| Backend | Isolation | Snapshots | Where it runs | When to use |
|---|---|---|---|---|
| `engram-sandbox-process` | **None** — host subprocess | Tarball of workdir | Anywhere (macOS, Linux) | Iterating on the orchestration layer without firing up a VMM. |
| `engram-sandbox-vz` | microVM (Hypervisor.framework) | APFS clone of rootfs | macOS 12+ on Apple Silicon | Mac dev with real microVM isolation. Sub-second cold boot, sub-second cold resume. ADR 0003. |
| `engram-sandbox-firecracker` | microVM (KVM) | FC memory snapshot + UFFD lazy paging | Linux + KVM | Production. Real isolation, real resource enforcement, sub-100ms hot resume. |

`process` and `vz` are dev-side: `process` for the fastest possible iteration loop (no kernel, no VM, plain subprocesses), `vz` for fidelity to the FC code paths (same `engram-bootstrap` supervisor, same `engram-agentd` wire protocol, same warm-pool semantics). Production isolation is always Firecracker.

## Quick start (dev, macOS or Linux)

The pinned toolchain (Rust, `just`, `jq`, `sqlx-cli`, `psql`, `protoc`, `pkg-config`, `openssl`) lives in `flake.nix`. Both options work:

**With Nix (recommended)** — same toolchain hashes on macOS aarch64 and Linux x86_64:

```bash
nix develop      # drops you into a shell with everything pinned
```

If you use [direnv](https://direnv.net), `direnv allow` once and the shell auto-activates whenever you `cd` in. Don't have Nix? The [Determinate Systems installer](https://install.determinate.systems) is one line and uninstalls cleanly.

**Without Nix** — install Rust >= 1.80, Docker, and `just` ([install](https://github.com/casey/just)) yourself.

Either way, run the dev stack:

```bash
just dev          # postgres + coordinator with the subprocess backend
```

In another shell:

```bash
just smoke-health
just smoke-create
```

End-to-end exec round-trip:

```bash
# Enable an image first (one-time setup; replace with your bake's URI):
curl -X POST http://localhost:8090/api/enabled-images \
  -H 'content-type: application/json' \
  -d '{"image_uri":"localhost:5001/cortex/api:warm-1"}'

SID=$(curl -s -X POST http://localhost:8090/sessions \
  -H 'content-type: application/json' \
  -d '{"image":"localhost:5001/cortex/api:warm-1"}' | jq -r .session_id)

curl -s -X POST "http://localhost:8090/sessions/$SID/exec" \
  -H 'content-type: application/json' \
  -d '{"command":"uname -a && echo \"session=$ENGRAM_SESSION_ID\""}' | jq

curl -X DELETE "http://localhost:8090/sessions/$SID"
```

You'll see real `uname` output and the session id env var injected by the coordinator.

## Other recipes

```bash
just check              # fmt + clippy + tests (cargo nextest), the pre-push gate
just test               # all tests via cargo nextest
just psql               # psql into the dev Postgres
just db-reset           # destroy + recreate the dev DB
just dev-firecracker    # coordinator wired to the Firecracker backend (Linux + KVM only)
just dev-vz             # coordinator wired to the Apple Silicon backend (macOS only)
just clean-var          # rm -rf the local sandbox cwds + snapshots
```

## Sessions are bake-image sandboxes with hot+cold snapshot durability

A session is one bounded unit of agent work. It's two things on the wire: an `image` (the OCI URI of a baked rootfs) and an optional `harness` (which agent process to attach). The bake image's `/workspace` is the workspace; the platform doesn't run any git operations itself.

**Two snapshot tiers, one primitive (ADR 0005).** Hot snapshots (FC `memory.bin` + state file on Linux, APFS rootfs clone on VZ) live on local NVMe and serve same-host hot resume. Cold snapshots are the same payload tar+zstd-compressed and pushed to a `BlobStorage` backend (S3/GCS/local fs); they survive host loss and resume on any host with capacity.

```
create → Active                                          (live VM)
       → idle TTL → Idle                                 (hot snapshot, VM destroyed)
       → next prompt/exec → Active                       (hot resume, sub-second)
       ... loop ...
       → disk pressure / admin flush → ColdEvicted       (blob upload, local copy dropped)
       → next prompt/exec → Active                       (cold resume on any host: download + untar)
       ... loop ...
       → cold blob deleted / KEK lost / hard TTL → Dead  (terminal)
```

`POST /sessions/:id/resume` is a three-branch dispatcher: **hot** (Idle → local NVMe restore on the original host, sub-second), **cold** (ColdEvicted → download + untar + restore on any host with capacity), **410 Gone** (Dead → both tiers lost). `ensure_active` auto-resumes both Idle and ColdEvicted on the next exec/prompt/SSE-subscribe so callers don't have to know which tier they're in.

**Disk pressure is survivable.** The host-agent's disk-pressure detector polls `statvfs`; below a configurable threshold it runs the same `flush_session` primitive the admin endpoint exposes (cheap-drop pass first for already-replicated snapshots, then LRU full-flush). One code path, two triggers: tests + drain-before-redeploy ops scenarios fire flush via `POST /api/admin/flush-idle`, production fires it via the detector.

**Agents push code from inside the sandbox** when their task is code-edit-shaped. Mount `GITHUB_TOKEN` (or an SSH key) via the image manifest's `[secrets.GITHUB_TOKEN]` block; the secret flows into the harness's env via the same pipeline that delivers `CLAUDE_CODE_OAUTH_TOKEN`. The platform doesn't care whether the agent runs `git push` or `slack.post()` or anything else — that's the agent's job, not Engram's.

**What survives what:**

| Event | Snapshot residency | Conversation log | In-memory state |
|---|---|---|---|
| Hot-suspend (idle eviction, same host) | hot, local NVMe | preserved | preserved (FC `memory.bin`) |
| Disk-pressure or admin flush | cold, blob storage | preserved | preserved (snapshot bytes) |
| Cold resume on a different host | local copy materialized on new host; cold blob still there | preserved | preserved (restored from snapshot) |
| Cold blob deleted / KEK lost / Dead | gone | preserved (Postgres) | lost |
| Mid-run host crash (no completed snapshot) | gone | up to last persisted event | lost |

## Building images

Engram images are baked from a `Dockerfile` + `engram.toml` in your repo. Dockerfiles handle "what's installed"; engram.toml carries engram-specific config — workspace-level secrets schema (NPM_TOKEN, GITHUB_TOKEN, etc.), network policy, resources. Harness-level credentials (`CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`, …) live one layer above the image and aren't declared here; the dashboard handles them per-harness at session-create time. See `DESIGN.md` for the full split.

```
my-repo/
├── Dockerfile
├── engram.toml
└── ... your code
```

```toml
# engram.toml
name = "cortex-api"
secret_mode = "literal"   # use "broker" in production (proxy not yet wired — see DESIGN.md)

[env]
NODE_ENV = "development"

# Workspace-level secret. The agent's `git push` to the checkpoint
# branch needs this; harness creds (Claude OAuth / API key) belong
# above the image, not here.
[secrets.GITHUB_TOKEN]
allow_hosts = ["api.github.com"]
required = true

[resources]
suggested_memory_mib = 4096
```

Bake it:

```bash
# Directory rootfs (dev backend, default)
engram image build --repo cortex/api --source ./path/to/repo

# ext4 rootfs for Firecracker
engram image build --repo cortex/api --source ./path/to/repo --format ext4
```

`--format ext4` produces `<images_dir>/<repo>/<tag>/rootfs.ext4` (a block-device image Firecracker mounts directly), built via `mke2fs -t ext4 -F -d` from the staged Docker export — no loopback mount, no root needed. The `Directory` default produces `<images_dir>/<repo>/<tag>/rootfs/` for the dev backend.

Then create a session against it (the coordinator picks up the new tag automatically):

```bash
SID=$(curl -s -X POST http://localhost:8090/sessions \
  -H 'content-type: application/json' \
  -d '{"repo":"cortex/api","branch":"main"}' | jq -r .session_id)
```

Requires Docker on the host. Compatible with Docker Desktop, OrbStack, Colima, and Podman with the docker-compat shim. See `DESIGN.md` for the full image / secret model.

## CLI

The `engram` CLI talks to the coordinator's HTTP API. `--json` on any read command emits raw JSON for piping into `jq` / scripts.

```bash
engram session list                                   # active sessions, table view
engram session get <id>
engram session delete <id>
engram session logs <id> --since 0                    # tail SSE event log; resume after idx
engram image build --repo <r> --source .              # bake (directory rootfs, default)
engram image build --repo <r> --source . --format ext4  # bake ext4 image for Firecracker
```

`ENGRAM_ENDPOINT` and `ENGRAM_TOKEN` (when auth is on) configure where the CLI talks and what it sends in `Authorization: Bearer ...`. The CLI defaults to `http://localhost:8080`; the `just dev` setup binds to `:8090`, so set `ENGRAM_ENDPOINT=http://localhost:8090` in dev.

## Auth

Set `ENGRAM_AUTH_TOKENS` (comma-separated) on the coordinator to require bearer-token auth on every endpoint except `/healthz`. Empty = auth disabled (dev default). The CLI carries `ENGRAM_TOKEN` automatically.

```bash
# coordinator
ENGRAM_AUTH_TOKENS=alpha,beta cargo run -p engram-coordinator
# client
ENGRAM_TOKEN=alpha engram session list
```

## Running on real Firecracker

The dev backend is for orchestration iteration. Real Firecracker needs Linux + KVM:

- A Hetzner AX-series box (~€40/mo, no nested-virt tax)
- A GCE n2d / n2 / c3 instance with `--enable-nested-virtualization`
- A Linux laptop or workstation
- CI

`just dev-firecracker` runs the coordinator against the Firecracker backend. Set `ENGRAM_KERNEL_IMAGE_PATH` to a vmlinux on disk; the rootfs comes from images baked with `--format ext4` (and, for `exec_stream`, with `engram-agentd` injected — see `crates/engram-image-builder/src/lib.rs::AgentInjection`).

The crate ships a five-test integration suite that covers the full surface against real microVMs:

```bash
# On a Linux + KVM host, after building engram-uffd-handler + engram-agentd:
bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh all
```

Tests: `boot` (kernel banner on serial), `lifecycle` (create/list/destroy round-trip), `snapshot` (file-backed restore), `snapshot_uffd` (lazy paging via the userfaultfd handler), `exec_real_vm` (bake → boot → exec through in-guest agent over vsock).

For UFFD-backed restore, the coordinator's host needs:
- `/dev/userfaultfd` mode 0666 (set via udev rule, see `dev-vm/scripts/bootstrap-remote.sh`)
- `vm.unprivileged_userfaultfd = 1` sysctl
- `engram-uffd-handler` binary on `$PATH` (or set `FirecrackerConfig::uffd_handler_bin`)

For agent-baked images, the coordinator needs:
- A static-musl `engram-agentd` build:
  `cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release`
- `init=/sbin/engram-init` in the kernel boot args (set via `FirecrackerConfig::default_boot_args`)

## License

Apache-2.0 — see `LICENSE`.
