# Engram

Self-hosted, open-source orchestrator for ephemeral AI agent sandboxes.

Engram orchestrates [Firecracker](https://github.com/firecracker-microvm/firecracker) microVMs on Linux production hosts and adds the layer above them: warm pools, snapshot lifecycle (UFFD-backed restore), multi-host scheduling, and pluggable cloud / blob-storage backends. A subprocess-based dev backend lets the entire orchestration layer run on macOS Apple Silicon during development; production isolation is always Firecracker.

It brings the Modal/E2B/Ramp-Inspect "ephemeral sandbox per task" pattern to open source so any organization can run their own without vendor lock-in. See `DESIGN.md` for the full architecture and roadmap.

## Status

**Phase 1** — orchestration layer end-to-end on the dev backend. The coordinator HTTP API, scheduler, warm pool, blob storage, persistent session-event log with SSE replay (`Last-Event-ID` / `?since=N`), bearer-token auth, image baker (Dockerfile + `engram.toml` → registry rootfs), pluggable secret store (env / dotenv / GCP-stub), and metadata store are real. End-to-end create → exec → delete works locally on macOS Apple Silicon via `engram-sandbox-process`, with per-exec wall-time accounting. CLI covers session list/get/delete/logs and image build.

**Phase 2 — done.** `engram-sandbox-firecracker` drives real Firecracker microVMs end-to-end: typed HTTP client over the FC unix socket; `create`/`destroy`/`list`/`snapshot`/`restore`/`exec_stream` all wired and exercised by integration tests on a Linux dev VM. `restore` supports both `File` mode (synchronous read of `memory.bin`) and `Uffd` mode (lazy paging via the new `engram-uffd-handler` companion process — sub-100ms resume). `exec_stream` reaches an in-guest `engram-agentd` over Firecracker's vsock proxy; the image baker injects a static-musl agent + init shim into ext4 rootfs images. 292 tests pass on macOS, plus 5 microVM integration tests on the dev VM.

**Phase 3 — done.** Multi-host coordinator: host-agents run as a separate binary that dials the coordinator over WebSocket (bincode-over-WS with hand-rolled `request_id` demuxer; `--mode=all` keeps single-binary `just dev` working by registering the local backend in-process). `HostRegistry` ranks hosts by snapshot affinity → warm pool → capacity; create/restore call `assign_session_host` so subsequent access routes directly. Cold-tier blob restore unblocked. `LISTEN/NOTIFY` on `session_events` + `host_dead` lets coordinator replicas re-broadcast events into local SSE subscribers and drop dead hosts in lockstep. `SessionStatus::PendingReassign` + `POST /sessions/:id/migrate` for operator-initiated transitions; **the dead-host auto-detector** (`crates/engram-coordinator/src/dead_host.rs`) handles the `kill -9 host → migrate within 30s` deliverable, racing replicas via `pg_try_advisory_lock` so exactly one wins each eviction. New `GET /api/hosts` + `engram host list/get/drain` CLI. 328 tests pass on macOS (up from 292). Remaining deferred items (W3C trace context on the wire, host-side Pool relocation so heartbeats carry real warm-pool data, `0003_host_assignments` reconciliation, live-Postgres HA test) are listed under Phase 3 in `DESIGN.md`.

**Phase 4 — cloud abstraction & spot tolerance.** Next.

## Workspace

```
crates/
  engram-core                       # types, traits, errors. No I/O.
  engram-protocol                   # wire types: bincode-over-WS Frame protocol (coordinator <-> host)
  engram-coordinator                # binary: HTTP API + scheduler
  engram-host-agent                 # binary: per-host daemon
  engram-image-builder              # binary: warm-image baker (Directory + Ext4 modes)
  engram-cli                        # binary: ops/admin tool
  engram-agentd                     # binary: in-guest exec daemon (vsock + UDS)
  engram-sandbox-firecracker        # SandboxBackend: Firecracker microVMs (production)
  engram-sandbox-process            # SandboxBackend: host subprocesses (DEV ONLY, no isolation)
  engram-uffd-handler               # binary: userfaultfd page-fault handler for fast snapshot restore
  engram-cloud-{gcp,static,mock}    # CloudBackend impls
  engram-storage-{gcs,s3,local}     # BlobStorage impls
  engram-secrets-{dev,gcp}          # SecretStore impls (env/dotenv; GCP Secret Manager)
  engram-postgres                   # MetadataStore impl
```

## Sandbox backends

The orchestration layer is VMM-agnostic — anything that implements `SandboxBackend` plugs in. We ship two:

| Backend | Isolation | Snapshots | Where it runs | When to use |
|---|---|---|---|---|
| `engram-sandbox-process` | **None** — host subprocess | Tarball of workdir | Anywhere (macOS, Linux) | Local dev. Iterating on the orchestration layer without firing up a VMM. |
| `engram-sandbox-firecracker` | microVM (KVM) | Full, file-backed or UFFD restore | Linux + KVM | Production. Real isolation, real resource enforcement, real snapshot/restore. |

The dev backend is for fast iteration; it deliberately doesn't try to mimic production isolation. Use a Linux box (CI, Hetzner, remote dev VM) to validate against Firecracker when fidelity matters.

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
  -d '{"repo":"hello","branch":"main"}' | jq -r .session_id)

curl -s -X POST "http://localhost:8090/sessions/$SID/exec" \
  -H 'content-type: application/json' \
  -d '{"command":"uname -a && echo \"session=$ENGRAM_SESSION_ID\""}' | jq

curl -X DELETE "http://localhost:8090/sessions/$SID"
```

You'll see real `uname` output and the session id env var injected by the coordinator.

## Other recipes

```bash
just check              # fmt + clippy + tests, the pre-push gate
just test               # all tests
just psql               # psql into the dev Postgres
just db-reset           # destroy + recreate the dev DB
just dev-firecracker    # coordinator wired to the Firecracker backend (Linux + KVM only)
just clean-var          # rm -rf the local sandbox cwds + snapshots
```

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
