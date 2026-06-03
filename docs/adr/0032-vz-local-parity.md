# ADR 0032: VZ ↔ FC parity for local (macOS) runs

Status: 2026-06-03 — **Accepted.** Driving the full stack on a Macbook
(`just dev` → VZ backend) end-to-end surfaced five gaps that broke the
enable-image → create → exec → shell → snapshot → resume lifecycle on the
Virtualization.framework (VZ) backend. Production runs on Firecracker (FC); VZ is
the dev-only backend that exercises the *same* coordinator/host-agent code paths
(see [[feedback-production-drives-design]]). Each gap was a place where a
host-agent wiring step, a base-snapshot invariant, or an agentd handshake assumed
FC's capabilities (vsock multiplexing, memory snapshots, a readiness port) that
VZ — virtio-console, clone-snapshot, cold-boot — doesn't share. The fixes make
the shared design accommodate a cold-boot/disk-only backend rather than forking a
VZ-specific path.

Builds on ADR 0003 (Apple Silicon backend: virtio-console, clone snapshots,
cold-boot restore), ADR 0020/0021 (per-image base snapshots + residency), and
ADR 0024 (`just dev`). No change to the FC/prod path.

## Context

Session create has exactly one path — enable-image captures a base snapshot,
`create_session` restores it (`restore_base_for_session`); there is no cold-boot
fallback (`api/sessions.rs`). So every gap below was a hard blocker, not a
degraded mode. The whole lifecycle now works on VZ: an image enables, a session
boots and execs with session env, the SHELL tab reaches ttyd, and an
idle-evict → resume cold-boot preserves on-disk state (including writes the guest
hadn't explicitly `sync`'d).

## The five gaps and fixes

1. **Host-agent never wired the chunk store into the VZ backend.** `main.rs`
   built `VzBackend::new(...)` without `.with_chunk_store(cs)` (the FC branch and
   the coordinator's own VZ branch both wire it). So VZ `snapshot()` ran with
   `chunk_store == None`, produced `disk_manifest = None`, and the coordinator's
   post-capture HEAD-verify rejected the enable ("chunked manifests failed
   HEAD-verify"). Fix: hoist `blob` above the backend match and wire
   `.with_chunk_store(ChunkStore::new(blob.clone()))` on the VZ branch.

2. **The base-snapshot memory manifest was required end-to-end** (PG `NOT NULL`
   since migration 0043, the `EnabledImageRef` heartbeat wire field, host
   residency advertisement, prefetch, enable flow). VZ cold-boots and captures no
   memory image, so it never produces one. Fix: make it optional everywhere —
   migration 0049 (nullable), `row.rs` decode → `Option`, the wire field →
   `Option<ManifestRef>`, advertisement passes `None` through, `image_prefetch`
   skips the memory tier when absent, and the enable flow no longer demands it.
   FC still populates it; disk residency (migration 0042) stays required for both.

3. **agentd's readiness handshake is meaningless on virtio-console.** agentd
   dials a ready port (1027) to unblock the host's `wait_agent_ready` — FC's
   per-sandbox `agent_ready` watch. The VZ console device only configures the
   agentd/bootstrap/harness data ports; there is no 1027 device, so the dial can
   never connect and agentd spun ~90 s retrying *before* it entered its accept
   loop. During that window the host's `guest_ip` + `start_agent` requests piled
   up on the single console byte stream and desynced (`start_agent` read a
   `GuestIp` reply for its `SpawnHarness`). Fix: a `Transport::supports_ready_port()`
   capability (vsock → true, console → false); agentd skips the handshake when
   false and serves RPCs immediately.

4. **VZ snapshots silently lost un-`sync`'d guest writes.** The clone-snapshot
   pauses vCPUs then APFS-clones the rootfs — but pausing doesn't flush the
   guest's dirty page cache to the virtio-blk disk (FC's memory snapshot carries
   those pages; VZ has none). A write not yet on disk at clone time was lost on
   the cold-boot restore. Fix: a `WireRequest::Sync` agentd RPC (`sync(2)`); VZ
   `snapshot()` calls it (best-effort, 5 s cap) while the VM is still running,
   before pause+clone, so the clone captures just-written state.

5. **The SHELL tab never started ttyd.** VZ inherited the trait-default
   `start_shell` (`Ok(7681)`), which promises a listener nothing spawned, so
   `proxy_shell` hit `connection refused`. FC forwards to agentd's `StartShell`
   RPC, which lazily spawns ttyd and probes it. Fix: override `start_shell` on
   `VzBackend` to forward the same RPC (host-side only — agentd already carries
   ttyd + the handler).

## Out of scope (intentional FC-only, not regressions)

VZ does not enforce `network.allow_hosts` (macOS NAT is opaque — open egress +
one-time warn), has no netns model, and cold-boots instead of UFFD lazy memory
restore. These are ADR 0003 design decisions and don't block local dev.

## Follow-ups surfaced while driving (not fixed here)

- **Mutable-tag image-cache staleness.** The host `ImageCache::ensure_image`
  returns the cached tag→digest without revalidating against the registry, so a
  re-baked `:warm-1` serves stale content until the cache is cleared. It bites
  every local re-bake. A targeted fix (revalidate the manifest digest for tag
  references) is the right follow-up; the workaround is a fresh tag per bake.
- **`capacity_total_mib = 0` on macOS hosts** — capacity is seeded from
  `/proc/meminfo`, absent on Darwin. Didn't block scheduling here but the
  scheduler is flying blind on a VZ host.
- **README `/api/...` examples are stale** — the HTTP API moved under `/api/v1`
  (ADR 0031); the quick-start curls 404.

## Verification

On a running `just dev` (VZ): `POST /api/v1/enabled-images` → 201; `POST
/api/v1/sessions` → active; `exec` returns `uname` + a populated
`ENGRAM_SESSION_ID`; the SHELL WS upgrades and ttyd streams its title frame; a
write with **no** explicit `sync` → snapshot → evict-local → resume reads back
intact. A macOS-only e2e test (`crates/engram-host-agent/tests/e2e_vz.rs`) locks
this lifecycle in.

**Commit chain.** _(to be filled in on merge)_
