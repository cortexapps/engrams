# Known issues

Things that work today but are quietly wrong, or shortcuts taken under
deadline. Each item has the file/line of the offending code and a
sketch of the proper fix so a future visitor (you, me, an agent) can
land it without rediscovering the problem.

## 1. ~~Warm-pool key ignores `repo`~~ (FIXED)

**Resolved.** `PoolKey` now carries `rootfs_source` alongside
`image_version`. Two specs with the same `image` tag but different
rootfs paths (the original `local://demo/warm-1` vs
`local://claude-oauth/warm-1` collision) now hash to distinct keys,
so the second checkout can't silently return a sandbox configured
with the first spec's rootfs.

In the OCI production path the image cache rewrites `rootfs_source`
to a content-addressed digest path before pool keying, so two
sessions sharing one OCI image_uri still share a single warm slot —
the original Phase 3 efficiency property is preserved.

Wire shape (`WarmPoolReport`) is unchanged; the disambiguator lives
entirely host-local. Regression at
`crates/engram-host-agent/src/pooled_backend.rs::tests::same_image_tag_but_different_rootfs_does_not_collide`.

## 2. VZ disk-attach failures leak the cloned rootfs

**File**: `crates/engram-sandbox-vz/src/backend.rs` — the `create`
path around the `clone_or_copy → VzVm::new → vm.start` sequence
(approximately lines 240-280).

Cleanup is wired only on `vm.start().await` failure:

```rust
clone_or_copy(&bake_rootfs, &rootfs_path).await?;
let (vm, port_fds) = VzVm::new(vm_cfg)?;          // <-- failure here leaks rootfs_path
if let Err(e) = vm.start().await {
    let _ = tokio::fs::remove_file(&rootfs_path).await;
    return Err(e.into());
}
```

When `VzVm::new` returns `Err` (the disk-attach call we hit during
the macOS Tahoe sector-alignment investigation), the just-cloned
`rootfs_path` is left behind in `work_dir`. After a few failed
creates we accumulated 4× 1.79 GB orphans in `var/sandboxes/`. On a
production host with retries against a transient VZ flake this would
fill the disk.

**Fix**: wrap the `VzVm::new` + `vm.start` block so any error path
from either step removes `rootfs_path`. Equivalently, use a guard
struct with `Drop` that unlinks the clone unless explicitly
`disarm()`d after `vm.start()` succeeds.

## 3. ~~Resume path drops the secret-augmented env~~ (FIXED)

**Resolved.** Commit `2d45a3b` (`fix(resume): re-resolve manifest
[secrets.*] from SecretStore`) landed the two-layer resume env build:
manifest secrets re-resolved fresh from the SecretStore + per-request
overrides decrypted from `session_secrets`. See
`crates/engram-coordinator/src/api/snapshot.rs::resume_from_fc_snapshot`
+ `resume_from_cold` (which use the same helpers).

## 4. Browser shell endpoint has no auth gate

**File**: `crates/engram-coordinator/src/api/shell.rs` — `GET /sessions/:id/shell`.

The dashboard's `SHELL` tab opens a WebSocket through the coordinator
to `ttyd` running inside the session's guest VM, giving the user an
interactive `bash` PTY against `/workspace`. The endpoint sits behind
the coordinator's existing bearer-token middleware, so production
deployments with `ENGRAM_AUTH_TOKENS` set are fine. **Dev** runs
without auth, which means anyone with network access to the coordinator
(by default `127.0.0.1:8090`, but trivially exposed via `--bind-addr
0.0.0.0:...`) can drop into a shell on any live session.

**Fix**: don't expose the dashboard publicly without
`ENGRAM_AUTH_TOKENS`. If we want a stronger guard, add a per-session
shell capability token issued at create-time and verified on the
upgrade — same shape as the existing harness attach token. Out of
scope for the current PR; the shell feature ships as a dev tool.

## 5. ~~Browser shell only works on the VZ backend~~ (FIXED)

**Resolved.** FC now provisions a per-VM `/30` with a TAP
terminated on the host (no shared bridge, no DHCP, static IP via
kernel `ip=` cmdline) and overrides `SandboxBackend::guest_ip` so
the dashboard SHELL tab works.

`manifest.network.allow_hosts` enforcement moved to a host-side
TLS-MITM proxy (`engram-egress-proxy`) — see
`crates/engram-egress-proxy/` for the substitution / SNI peek /
CA-and-leaf logic and `crates/engram-sandbox-firecracker/src/net.rs`
for the static iptables ruleset that REDIRECTs VM→tcp/443 to the
proxy. Coverage: `engram-egress-proxy/tests/intercept_e2e.rs`
(loopback MITM), `engram-sandbox-firecracker/tests/host_startup.rs`
(sudo, iptables apply), `tests/harness_loopback.rs` (CA delivery
via substrate).

## 6. ~~Egress proxy lives on the coordinator, not the host-agent~~ (FIXED)

**Resolved.** ADR 0006 landed the host-agent-owned proxy topology.
The coordinator no longer runs the egress proxy; each FC host-
agent spawns its own (`engram_host_agent::egress::HostEgress`),
loads the deployment CA via a pluggable `CaSource` trait (env /
local-disk / GCP Secret Manager + Workload Identity), and stamps
the CA cert into every harness substrate it builds.

Per-session policy ships from the coordinator over the existing
WS via `NotifyKind::SessionEgressPolicy`, applied to the local
proxy registry before the harness starts. Broker mode now works
end-to-end. See [ADR 0006](./adr/0006-host-agent-egress-proxy.md).

## 7. Idle auto-eviction doesn't fire in `--mode=coordinator`

**Files**: `crates/engram-coordinator/src/idle_evictor.rs`,
`crates/engram-host-agent/src/harness.rs`.

The idle evictor reads `state.harness_hub.idle_sandboxes(...)`. In
`--mode=all` (dev/single-host) the hub sees every harness via the
local sandbox backend's sink and the evictor works. In
`--mode=coordinator` the `HarnessHub` lives on the coordinator but
its source — `set_harness_sink` on the `RemoteSandboxBackend` —
is the trait default no-op. The hub stays empty, `idle_sandboxes`
returns nothing, and no sessions get auto-suspended on idle.

The driver still runs and is harmless (no false evictions); it
just produces no candidates. Operators can manually suspend via
`POST /api/admin/sessions/:id/flush` or
`POST /api/admin/flush-idle`.

**Fix** (v2): instantiate a `HarnessHub` inside the host-agent's
`run` loop, wire the local backend's harness sink into it, ship
`SessionId` to the host-agent so it can `bind_session` locally,
poll `idle_sandboxes` there, and emit candidates over WS via a
new `NotifyKind::IdleEvictionCandidates(Vec<(SessionId, SandboxId)>)`.
The coordinator's `evict_idle_session` pipeline function is
already split out and can stay where it is — only the driver
moves.

## 8. No system-wide event stream on the coordinator

The dashboard's Overview page only polls `GET /sessions` and
`GET /api/hosts` at 1Hz — no SSE. Live event streaming is reserved
for the SessionDetail page (one SSE per page-view, scoped to the
session being read). That's deliberate: the coordinator's only
event endpoint today is `GET /sessions/:id/events`, so a "live
ticker across all sessions" would mean N concurrent EventSource
connections from the browser, hitting the 6-per-origin HTTP/1 cap
and starving the rest of the app.

**Future work**, if a cross-session live feed is wanted: add
`GET /events` on the coordinator that fan-ins the per-session
broadcast buses into one SSE feed. That'd let a dashboard show
real-time activity across the whole system over a single connection.
Out of scope until there's a concrete need.

## 9. ~~NBD disk adapter for Firecracker not implemented~~

**Resolved**: Phase 4 NBD daemon shipped (commits `55dd889`,
`c770b6f`, `645afd8`, `e4f7500`, `94e9a52`). Host-agent's
`PooledBackend` prefers NBD over materialize-to-file when
`ENGRAM_NBD_DEVICES=/dev/nbd0,…` is set in the env (Packer
manifest + Terraform `fc-host-mig` module both wire this).
Direct kernel ioctls — no `nbd-client` userspace required.
Linux-gated CI test at
`crates/engram-host-agent/tests/nbd_chunked_disk.rs` boots a
real microVM against `/dev/nbd0` served by the daemon.

## 10. ~~UFFD-from-chunks for memory not implemented~~

**Resolved**: Phase 5 UFFD-from-chunks + canonical-base
MAP_PRIVATE + working-set record-and-replay shipped. Image-
builder captures the canonical memory snapshot during bake
(`BuildRequest.capture_canonical_memory` opt-in) and chunks
it into the store. UFFD handler resolves session faults via
the chunk store, prefaults the working-set trace before vCPUs
unfreeze, and records the actual access trace for subsequent
restores. `FirecrackerBackend::{snapshot,restore}` thread
`canonical_memory_manifest` + `memory_manifest` end-to-end.
PooledBackend materializes `memory.bin` from chunks on
cross-host restore when not present locally.

## 11. ~~`snapshots` table still carries cold-tier columns~~

**Resolved**: Phase 7 (ADR 0007) shipped. Migration `0020_drop_cold_tier.sql`
dropped the cold-tier columns (`local_path`, `blob_present`,
`replicated_at`, the envelope-encryption quartet) + the
`cold_evicted_at` column on `sessions`. `MetadataStore::flush_to_cold` /
`clear_local_path` / `latest_cold_snapshot_for_session` /
`list_idle_sessions` trait methods deleted. `SessionStatus::ColdEvicted`
deleted. `SealedBlobRef` deleted from `engram-core::traits`. The
chunked manifest refs are the single durability primitive.

## 12. No metrics on the chunked-storage code paths

Zero observability on the new pieces: cache hit rate, chunk
fetch latency, materialize time, GC counts, manifest puts/gets.
Operators can't tell whether sessions are slow because chunks
are missing the cache, GCS is slow, or materialization is
contending on the local mutex.

**Future work**: add a tracing-based metrics layer that
exports Prometheus-compatible counters/histograms on
`ChunkCache::get`, `ChunkStore::{put,get}_manifest`,
`materialize_chunked_rootfs`, the chunk GC sweep, etc.
Framework choice (Prometheus exporter vs OpenTelemetry) is a
separate decision; today's stack uses structured tracing
without an exporter.

**Tracked**: `docs/chunked-storage-rollout.md` Tier 4 #7.

## 13. ~~Materialized-rootfs files leak between manifest updates~~

**Resolved**: `engram_host_agent::orphan_reap::reap_materialize_dir`
shipped as both a library primitive and a `POST /api/admin/reap-materialize-dir`
admin endpoint. Parses `<manifest_id>-vN.ext4` filenames + deletes
any not referenced by a live `disk_manifest_id` row; `min_age_secs`
guard protects in-flight clonefiles. The chunk-store GC scheduler
(`engram_coordinator::chunk_gc`) drives this on a cadence. Multi-
host fanout shipped via WS-RPC in commit `2971115` (WIRE v3 /
`HostAdminHandler::reap_materialize_dir`); in `--mode=coordinator`
the admin endpoint walks every connected host and aggregates per-
host outcomes.

## 14. Wire compatibility is enforced at hello but bincode-positional

`engram-protocol::WIRE_VERSION` (v4 today) + the hello-frame
handshake reject coord/host-agent version mismatches loudly.
The handshake itself works.

Version history (current trajectory):
- v1 — `SnapshotMetadata.disk_manifest` add (`e68ee23`)
- v2 — `RequestKind::ResolveRegistryAuth` for OCI auth WS-RPC (`ad13dc0`)
- v3 — `RequestKind::ReapMaterializeDir` + `WireReapStats` for
  multi-host materialize-dir reap fanout (`2971115`)
- v4 — Phase 6 destructive trait reshape: `Snapshot` drops
  `dest_path`; `Restore` takes `metadata: SnapshotMetadata` (`b8afb42`)

What's *not* a known issue but worth knowing: bincode is
schemaless positional encoding, so any future serde-derived
field add/remove anywhere in the wire types is a hard break
that must bump WIRE_VERSION. Future contributors editing
`engram-protocol::wire` should bump the version and add a
history note alongside any structural change.

## 15. ~~Cross-namespace bricked images: chunks live in BlobStorage, OCI artifact is just metadata~~ (FIXED, ADR 0008)

**Resolved.** ADR 0007's chunked-storage push (the `d79094f`
"skip the rootfs.ext4 layer when bundle.json is present"
optimization) made the OCI artifact a pointer-only — chunks
lived exclusively in the bake's `BlobStorage` namespace.
Result: a bake in namespace A and a push to a registry
produced an artifact that pulled cleanly into namespace B but
404'd at chunk-fault time when sessions tried to read a chunk
that wasn't in B's BlobStorage. The failure was silent at
push and pull, only surfaced on first chunk fault.

ADR 0008 closes this by making the OCI registry the durable
source of truth for image chunks via Nydus-shaped layers
(bootstrap + chunk_blob), with a tiered fault path that
falls through `local NVMe → BlobStorage → OCI`. A host with
an empty BlobStorage namespace now Range-GETs missing chunks
from the registry on demand and CDN-fills BlobStorage as a
side effect. Per-host cold start has a one-time fault cost;
warm steady-state hits the cache tier.

Wiring: `BlobStorageResolver` + `OciChunkResolver` composed
via `TieredChunkResolver` (`7279232`); `PooledBackend`
constructs the tiered resolver per-sandbox in
`resolve_rootfs` when `CachedImage::is_disk_chunked_oci()`
and an `OciClient` is wired (`a2b7bdd`). Integration test at
`crates/engram-host-agent/tests/chunked_oci_fault.rs`
exercises the cold-fault → CDN-fill → warm-cache cycle
against a loopback fake registry; runs in CI with zero
external deps.

See `docs/adr/0008-chunks-in-oci.md` for the full
architectural picture and the five-phase rollout
(`b04834b → 7279232 → 77d53f9 → fb719dd → 4cdbebd → d711697
→ 73ab684 → fc9437c → a2b7bdd`).
