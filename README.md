# Engram

Self-hosted, open-source orchestrator for ephemeral AI agent sandboxes.

Engram orchestrates [Firecracker](https://github.com/firecracker-microvm/firecracker) microVMs on Linux production hosts and adds the layer above them: warm pools, snapshot lifecycle (UFFD-backed restore), multi-host scheduling, and a pluggable cloud abstraction. A subprocess-based dev backend lets the entire orchestration layer run on macOS during development; an Apple Silicon backend (Apple Virtualization.framework) gives Mac devs real microVM isolation locally. Production isolation is always Firecracker.

It brings the Modal/E2B/Ramp-Inspect "ephemeral sandbox per task" pattern to open source so any organization can run their own without vendor lock-in. See `DESIGN.md` for the full architecture and roadmap.

## Status

**Phase 1 — done.** Orchestration layer end-to-end on the dev backend. Coordinator HTTP API, scheduler, warm pool, persistent session-event log with SSE replay (`Last-Event-ID` / `?since=N`), bearer-token auth, image baker (Dockerfile + `engram.toml` → rootfs), pluggable secret store (env / dotenv / GCP-stub), Postgres metadata store. End-to-end create → exec → delete on macOS via `engram-sandbox-process`. CLI covers session list/get/delete/logs and image build.

**Phase 2 — done.** `engram-sandbox-firecracker` drives real Firecracker microVMs end-to-end: typed HTTP client over the FC unix socket; `create`/`destroy`/`list`/`snapshot`/`restore`/`exec_stream` all wired and exercised by integration tests on a Linux dev VM. `restore` supports both `File` mode (synchronous read of `memory.bin`) and `Uffd` mode (lazy paging via `engram-uffd-handler` — sub-100ms resume). `exec_stream` reaches an in-guest `engram-agentd` over Firecracker's vsock proxy; the image baker injects a static-musl agent + init shim into ext4 rootfs images.

**Phase 3 — done.** Multi-host coordinator: host-agents run as a separate binary that dials the coordinator over WebSocket (bincode-over-WS with hand-rolled `request_id` demuxer + W3C trace context per RPC; `--mode=all` keeps single-binary `just dev` working by registering the local backend in-process). The warm `Pool` is host-side (`engram-host-agent::pooled_backend::PooledBackend`), so heartbeats carry real `(ready, target)` per `image_version` and `HostRegistry`'s scheduler ranks by snapshot affinity → warm pool → capacity. `LISTEN/NOTIFY` on `session_events` + `host_dead` lets coordinator replicas re-broadcast events into local SSE subscribers and drop dead hosts in lockstep. The dead-host auto-detector (`crates/engram-coordinator/src/dead_host.rs`) handles `kill -9 host → mark Dead within 30s`, racing replicas via `pg_try_advisory_lock` so exactly one wins each eviction. `sessions.sandbox_id` persistence + `repopulate_routing` on coord startup means Active sessions survive a coordinator restart. `GET /api/hosts` + `engram host list/get/drain` CLI shipped. Production hardening (TLS, `HostStatus::Disconnected`, backpressure tuning) lands with Phase 6.

**Phase 4 — done. Versioned conversations + pack hosts + one-shot task runner.** All tracks shipped; ADRs 0001 + 0002 capture the trajectory. Workspace + transcript are durable via git (`engram/sessions/<id>` checkpoint branches per session); hot-suspend keeps Firecracker memory snapshots for *local* idle eviction (Track B); harness protocol (`engram-harness-{proto,noop,claude}`) + in-VM supervisor (`engram-bootstrap`) wired end-to-end; `POST /sessions/:id/checkpoint` (manual) + auto-checkpoint on `HarnessEvent::Idle` / `RunCompleted` for one commit per completed agent run (Track C); preemption best-effort flush (Track D); `engram session {log,diff,fork,checkpoint}` CLI surface (Track F). ADR 0002 retired the cross-host cold-resume code paths in favor of explicit one-shot semantics: a session lives ↔ its FC snapshot, snapshot loss → `Dead` terminal state, caller forks the workspace to continue. The Claude Code adapter (`engram-harness-claude`) shipped as the reference adapter and runs end-to-end against real Firecracker microVMs.

**Phase 4.5 — done (April 2026). Apple Silicon backend.** `engram-sandbox-vz` drives Apple's Virtualization.framework directly from Rust via the `objc2-virtualization` bindings — no Swift driver. Mac contributors get real microVM isolation locally (not just subprocesses). Multi-port virtio-console replaces vsock for the host↔guest control plane (universal kernel support, no `CONFIG_VIRTIO_VSOCKETS=y` requirement) — `engram-transport` is a backend-agnostic trait abstraction over both. APFS `clonefile(2)`-based snapshots replace VZ's broken-upstream `saveMachineStateToURL`/`restoreMachineStateFromURL` pair (~50 ms even for 1.7 GB rootfs); the snapshot is the clone. Cold boot 562 ms, cold resume 746 ms, warm-pool checkout 30 ms — same warm-pool code path as FC. ADR 0003 captures the design.

**389 tests pass** via `cargo nextest run --workspace` (3 ignored: 2 live-Postgres integration tests, 1 entitlement-gated VZ smoke test), plus 5 Firecracker integration tests on the Linux dev VM and 14 VZ unit tests on macOS Apple Silicon.

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
  engram-host-agent                 # binary: per-host daemon (warm pool, harness hub, checkpoint primitive)
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
SID=$(curl -s -X POST http://localhost:8090/sessions \
  -H 'content-type: application/json' \
  -d '{"repo":"local://hello","branch":"main"}' | jq -r .session_id)

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

## Sessions are versioned conversations + one-shot tasks

A session is one bounded unit of agent work. It's bound to a writable git repo + base branch on create, and owns a checkpoint branch — `engram/sessions/<session_id>` — on that repo's remote. The agent's progress is durable via that branch.

**Engram is a one-shot task runner** (ADR 0002). A session lives ↔ its FC memory snapshot:

```
create → Active                    (live VM)
       → idle TTL → Idle           (FC snapshot taken, VM destroyed)
       → next prompt/exec → Active (hot resume from same-host snapshot)
       ... loop ...
       → snapshot invalidated      (host crash, disk full, hard TTL)
       → Dead                      (terminal)
       → caller forks the workspace if they want to continue
```

`POST /sessions/:id/resume` is a same-host operation: hot resume from the FC snapshot if it exists, `410 Gone` (`snapshot_invalidated`) if it doesn't. There is **no cross-host cold resume** — the orchestrator does not race to keep a session alive across host loss. The contract is intentionally narrow: agents externalize their state (workspace files + transcript), so the right durability primitive is git, not VM-memory replication.

**Auto-checkpoint cadence.** One commit per *completed agent run*. The harness emits `HarnessEvent::Idle` when it finishes a prompt; the host-agent's EventSink fires `checkpoint_workspace_only` (git add/commit/push). Reads as the agent's session log — exactly what you'd `git diff` or open a PR from.

**Checkpoint branches as forkable artifacts.** `engram session fork <id>` creates a new session whose checkpoint branch starts at the source's HEAD — to continue a Dead session's work, branch a session that went off the rails, or replay from any past `--at <event_idx>`. The original branch keeps existing for inspection.

**What survives what:**

| Event | Workspace files | Conversation log | In-memory state |
|---|---|---|---|
| Hot-suspend (idle eviction, same host) | preserved (in `/workspace`) | preserved | preserved (FC snapshot) |
| Snapshot invalidated / Dead | recoverable via `engram session fork <id>` | preserved (Postgres replay only — agent does not auto-resume) | lost |
| Mid-run preemption (no fresh checkpoint) | last completed run's state | up to last completed run | lost |
| Read-only / `local://` session host loss | lost | preserved | lost |

## Building images

Engram images are baked from a `Dockerfile` + `engram.toml` in your repo. Dockerfiles handle "what's installed"; engram.toml carries engram-specific config (secrets schema, network policy, resources):

```
my-repo/
├── Dockerfile
├── engram.toml
└── ... your code
```

```toml
# engram.toml
name = "cortex-api"
secret_mode = "literal"   # use "broker" in production

[env]
NODE_ENV = "development"

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
