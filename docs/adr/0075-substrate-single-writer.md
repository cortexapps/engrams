# 0075 — Substrate single-writer: one chunk-cache owner per host

Status: Accepted (2026-07-08)

Commit chain: Phases 1+2 landed together in #583 (`43c87b96`), review
fix folded in. ADR 0076 (`engram-substrated`) deliberately REMAINS
Proposed/gated per its own text — accepting this ADR does not start
the daemon work.

Issue: #547 (2026-07 core-ops overhaul, Tier 2). Depends on #528 (the
disk budget this makes enforceable). Companion: ADR 0076 (the
`engram-substrated` end state — design only, gated).

## Problem

The NVMe chunk-cache directory is shared mutable state across 1 + N
processes (host-agent + one `engram-uffd-handler` per Uffd sandbox),
each constructing its OWN `ChunkCache` over the same root — so
singleflight, the pin set, the eviction debounce, and the thrash
tracker are all process-local while the bytes and the budget are
shared. Content addressing makes concurrent same-bytes writes
idempotent, which made this look safe; it says nothing about unlink
(eviction), budget, or pin visibility. Consequences (2026-07-01
evidence): 583+55/7d write_local/write-through ENOENTs; a live hazard
where a handler sweep can evict host-agent-pinned base chunks (the
handler's pin map is empty and `evict_to_budget` skips only its own);
duplicate GCS fetches per process; the 92s ca82a6f1 resume class.
Every recent fix here (#437 writer-unique temps, #522 write-through,
#557's `eviction_enabled:false`) patched atomicity or halved the
symptom; none closed the coordination gap.

## Decision

**The cache directory has exactly one writing process — the
host-agent — enforced at the type level; every other process is a
reader that requests population.** Corollaries: one pin registry, one
singleflight, one eviction policy (the #528 budget), one thrash
metric.

- `engram-chunk-store` splits: `ChunkCache` (the writer, surface
  unchanged) + `ChunkCacheReader` (open/read/contains only — a handler
  that compiles cannot mutate the directory). The writer-unique
  `.partial` temp machinery stays for IN-process concurrency; its
  cross-process rationale comments are rewritten.
- New crate `engram-substrate-proto` (modeled on
  `engram-migrate-proto`): sync length-prefixed bincode framing, NO
  tokio/tonic (the client sits adjacent to the fault path — the
  `peer.rs` discipline), plus SCM_RIGHTS fd helpers. Messages:
  `Hello{proto_version, canonical_manifest} → HelloAck{base_staged,
  tmpfs_ok}`; `Populate{hash} → Populated{len}+fd | PopulateErr{msg}`.
  Socket: `<work_dir>/substrate.sock`, bound by the host-agent.
- Writer `Populate` semantics: pin → `cache.get(hash, fetch)` (global
  singleflight, verify-on-populate, budget sweep) → open O_RDONLY →
  send fd → unpin. Fd-passing (not inline bytes) makes a post-reply
  eviction unlink harmless.
- Handler read path: (1) `reader.read` hit → serve; (2) populate
  request → read returned fd; (3) LAST RESORT when the writer is
  unreachable past the retry budget (a rolling host-agent — ADR 0044
  K2 handlers outlive rolls): direct `store.get_chunk` served from
  memory, never written to the cache dir, counted + WARNed. The
  fallback preserves the invariant (read-only) at the cost of an
  uncached fetch.
- Readiness: the handler sends `Hello` and awaits `HelloAck` BEFORE
  creating the base-shm file / binding the FC-facing UDS (the
  `peer.rs` connect-before-bind pattern). The writer answers from
  probes (statfs TMPFS_MAGIC, cache-root writability, manifest
  staged), so an unready host fails at spawn time — closing the
  handler-side half of the post-roll `register memory … userfaultfd`
  window (#531 holds the heartbeat-level half).
- Phase 2: the host-agent pins the session's divergent manifest chunks
  for the sandbox lifetime (pin_all/unpin_all, the `image_prefetch`
  batch pattern) — restoring the property the stale cache.rs doc
  claimed existed, in the only place pins now mean anything.

## Non-goals (explicit)

No daemon (ADR 0076, gated); NBD stays in the host-agent; base-shm
stays multi-writer-idempotent (it is the exemplar, not the patient);
the `store.rs` put-side write-through hole belongs to #548; File-mode
machinery belongs to #530.

## VZ / Process backends

N/A by construction: neither spawns out-of-process cache clients (the
UFFD handler is FC/Linux-only), so the host-agent's in-process cache
is already the sole writer there. `engram-substrate-proto`'s fd
helpers are `cfg(target_os = "linux")`.

## Divergence log

- **One PR, not two.** Phases 1 and 2 landed together in #583
  (`43c87b96`) — the reader/writer split, `engram-substrate-proto`,
  the populate server, the handler's 3-step read path, and the
  readiness handshake are one coherent cutover; splitting them would
  have shipped a window where the handler compiles read-only but has
  no populate path.
- **Session-chunk pinning is per-connection, not sandbox-lifetime
  `pin_all`.** The sketch had the host-agent pinning the session's
  divergent manifest chunks for the sandbox lifetime via
  `pin_all`/`unpin_all` in the restore path. What shipped is better
  self-healing: the substrate server pins the `Hello` manifest's
  chunks for the CONNECTION lifetime (`conn_pins`, unpinned on every
  disconnect path), and the client replays `Hello` on every fresh
  dial (`with_conn`) — not just at startup — because the server's pin
  set and proto check are per-connection state, and the designed-for
  reconnect (a rolled host-agent's successor, holding an empty pin
  set) is exactly when silently skipping the replay would leave the
  handler unpinned for the rest of the sandbox's life. Pin release
  needs no destroy-path bookkeeping: the disconnect is the unpin.
- **ADR 0080 (landed after, 2026-07-07) added a writer-side caller,
  not a writer.** The enable-time rootfs materializer
  (`MaterializeImage`) chunks through the host-agent's ONE in-process
  `ChunkStore`/`ChunkCache`, so the single-writer invariant holds by
  construction. Out-of-scope note so nobody assumes otherwise: the
  materializer's scratch-headroom guard
  (`materialize.rs::estimated_peak_scratch_bytes` vs raw `statvfs`)
  measures the same volume as the cache but is deliberately NOT
  integrated with the cache's pin/evict bookkeeping — materialize
  scratch pressure is invisible to the cache evictor, bounded instead
  by the ≤1-concurrent-materialize cap + the pre-pull size cap +
  scrub-on-exit.
