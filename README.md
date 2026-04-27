# Engram

Self-hosted, open-source orchestrator for ephemeral AI agent sandboxes.

Engram orchestrates [Firecracker](https://github.com/firecracker-microvm/firecracker) microVMs on Linux production hosts and adds the layer above them: warm pools, snapshot lifecycle (UFFD-backed restore), multi-host scheduling, and pluggable cloud / blob-storage backends. A subprocess-based dev backend lets the entire orchestration layer run on macOS Apple Silicon during development; production isolation is always Firecracker.

It brings the Modal/E2B/Ramp-Inspect "ephemeral sandbox per task" pattern to open source so any organization can run their own without vendor lock-in. See `DESIGN.md` for the full architecture and roadmap.

## Status

Phase 1 — orchestration layer end-to-end on the dev backend. The coordinator HTTP API, scheduler, warm pool, blob storage, snapshot manager scaffolding, persistent session-event log with SSE replay (`Last-Event-ID` / `?since=N`), bearer-token auth, image baker (Dockerfile + `engram.toml` → registry rootfs), pluggable secret store (env / dotenv / GCP-stub), and metadata store are real. End-to-end create → exec → delete works locally on macOS Apple Silicon via `engram-sandbox-process`, with per-exec wall-time accounting on the response. CLI covers session list/get/delete/logs and image build. 240 tests, fmt + clippy clean.

Phase 2 — the production Firecracker backend (`engram-sandbox-firecracker`) is scaffolded as a typed stub: trait surface complete, every method returns a structured error pointing at the Firecracker endpoint to wire (`PUT /machine-config`, `PUT /snapshot/load` with UFFD, etc.). Implementation work in progress on a Linux dev box.

## Workspace

```
crates/
  engram-core                       # types, traits, errors. No I/O.
  engram-protocol                   # gRPC defs (coordinator <-> host)
  engram-coordinator                # binary: HTTP API + scheduler
  engram-host-agent                 # binary: per-host daemon
  engram-image-builder              # binary: warm-image baker
  engram-cli                        # binary: ops/admin tool
  engram-sandbox-firecracker        # SandboxBackend: Firecracker microVMs (production, stub)
  engram-sandbox-process            # SandboxBackend: host subprocesses (DEV ONLY, no isolation)
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
| `engram-sandbox-firecracker` | microVM (KVM) | Full + diff, UFFD restore | Linux + KVM | Production. Real isolation, real resource enforcement, real snapshot/restore. |

The dev backend is for fast iteration; it deliberately doesn't try to mimic production isolation. Use a Linux box (CI, Hetzner, remote dev VM) to validate against Firecracker when fidelity matters.

## Quick start (dev, macOS or Linux)

Requires Rust >= 1.80, Docker, and `just` ([install](https://github.com/casey/just)).

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
engram image build \
  --repo cortex/api \
  --source ./path/to/repo \
  --images-dir ./var/snapshots/images
```

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
engram session list                       # active sessions, table view
engram session get <id>
engram session delete <id>
engram session logs <id> --since 0        # tail SSE event log; resume after idx
engram image build --repo <r> --source .  # bake Dockerfile + engram.toml
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

## Validating the production path

The dev backend is for orchestration iteration. Real Firecracker validation needs Linux + KVM:

- A Hetzner AX-series box (~€40/mo, no nested-virt tax)
- A GCE n2d / n2 / c3 instance with `--enable-nested-virtualization`
- A Linux laptop or workstation
- CI

`just dev-firecracker` runs the coordinator against the Firecracker backend and refuses to start in environments where it can't bring a VM up. Phase 2 fills in the actual Firecracker integration; today it surfaces structured "not yet implemented" errors that point at the specific Firecracker endpoint to wire.

## License

Apache-2.0 — see `LICENSE`.
