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

## 9. NBD disk adapter for Firecracker not implemented

The chunked-storage rollout (ADR 0007) ships disk reads via a
"materialize-first" path: the host-agent's `PooledBackend`
assembles the chunked manifest into a per-host `.ext4` file
before VM boot, then attaches that file as FC's `path_on_host`.
Functional but pays cost proportional to image size on first
materialize — ~16 s for a 16 GiB rootfs against GCS.

The Phase 4 NBD path (`crates/engram-host-agent/src/disk_daemon.rs`,
~800 lines per the plan) would replace this with on-demand
block reads streamed from the chunk store. Sub-second
VM-running time even for cold-cache images.

**Tracked**: `docs/chunked-storage-rollout.md` Tier 4 #8.

## 10. UFFD-from-chunks for memory not implemented

The rollout's memory side is unimplemented. Sessions boot cold
from the kernel rather than restoring from a chunked canonical
memory snapshot, so first-session-on-a-host pays full boot
latency (~3–10 s depending on the image) and we don't get
cross-VM page-cache dedup. The image-builder doesn't yet
capture the canonical memory snapshot during bake either.

The full implementation is large (~1500 lines): bake-time
canonical capture in `engram-image-builder/src/canonical_boot.rs`,
a substantial rewrite of `engram-uffd-handler` to serve faults
from the chunk store + record/replay working-set traces, and
wiring through `FirecrackerBackend::{snapshot,restore}`.

**Tracked**: `docs/chunked-storage-rollout.md` Tier 4 #9.

## 11. `snapshots` table still carries cold-tier columns

ADR 0007 supersedes the two-tier durability model, but the
`snapshots` table schema (migration 0001 + 0016) still has
`local_path`, `blob_present`, and the envelope-encryption
quartet (`wrapped_dek`, `nonce`, `ciphertext`, `key_id`). The
chunked write path (commit `e68ee23`) produces a
`SnapshotMetadata.disk_manifest` that the coord captures in
memory but doesn't persist to the row.

Migration `0018_chunked_storage.sql` (Phase 6 of the rollout)
drops the cold-tier columns and adds `disk_manifest_id` +
`disk_manifest_version`. Until that lands, `SnapshotRecord`
reshape + the `MetadataStore` method retirement
(`flush_to_cold` etc.) are also blocked.

**Tracked**: `docs/chunked-storage-rollout.md` Tier 4 #5
(Phase 6 trait + DB reshape).

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

## 13. Materialized-rootfs files leak between manifest updates

`<work_dir>/chunked-rootfs/<manifest_id>-vN.ext4` accumulates one
file per (re)materialized manifest version. The
`PooledBackend::with_chunk_cache` wiring (commit `f42e1a2`)
covers the *chunk* read cache via LRU; the materialized files
themselves are never reaped.

For a host whose images turn over frequently (CI bakes a new
warm tag every day), the materialize dir grows without bound.
Fix needs a scanner that enumerates live manifest refs from the
DB and deletes any `chunked-rootfs/<manifest_id>-vN.ext4` not
in the set. Currently blocked on Phase 6's DB schema (issue
#11) — there's no `disk_manifest` column on `snapshots` to
enumerate yet.

**Tracked**: `docs/chunked-storage-rollout.md` Tier 4 #3(b).

## 14. Wire compatibility is enforced at hello but bincode-positional

`engram-protocol::WIRE_VERSION` (v2 today) + the hello-frame
handshake reject coord/host-agent version mismatches loudly.
The handshake itself works.

What's *not* a known issue but worth knowing: bincode is
schemaless positional encoding, so any future serde-derived
field add/remove anywhere in the wire types is a hard break
that must bump WIRE_VERSION. Future contributors editing
`engram-protocol::wire` should bump the version and add a
history note alongside any structural change.
