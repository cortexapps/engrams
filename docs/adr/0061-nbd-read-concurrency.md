# ADR 0061: NBD read concurrency — pipeline the serve loop

**Status:** Accepted (2026-06-29). The in-guest rootfs is served over NBD from the
chunked content-addressed store (ADR 0007) by a per-sandbox daemon whose serve loop
processes guest requests **strictly one at a time** — read a request, fully service it,
write the reply, then read the next — and whose per-read path fetches the chunks a
multi-chunk read spans **serially**. The kernel NBD client issues many concurrent
in-flight requests (each carries a `handle` for correlation), but the daemon collapses
that parallelism to a single in-flight op. This ADR pipelines the serve loop and
parallelizes the per-read chunk fetch, while preserving the snapshot-drain invariant the
serialization currently makes trivial. Implemented in this change (branch
`feat/adr-0061-nbd-read-concurrency`), behind `ENGRAM_NBD_SERVE_CONCURRENCY` (default 8,
`=1` kill-switch); the before/after `agent_handshake` p50/p95 + in-session-build
measurement is the post-roll validation.

**Related:** ADR 0007 (chunked store + the original NBD daemon), ADR 0049 (the `/dev/nbdN`
slot allocator), ADR 0018 (the `InFlightTracker` / `wait_idle` snapshot-drain contract),
ADR 0021 P2 + ADR 0039 + the image-prefetch supervisor (why the rootfs is NVMe-resident at
restore — see Context).

## Context

We surveyed five disk-substrate levers (concurrent serve, concurrent per-read fetch, a
disk-side working-set trace + restore prefault, sequential readahead, pinning the
per-session working set) and **measured prod before committing**. The measurement
collapsed the scope to the two concurrency levers:

- **The rootfs is already NVMe-resident at restore, not GCS-bound.** The per-host
  image-prefetch supervisor (`image_prefetch.rs`, ADR 0021 P2 / 0039) warms the full
  base-snapshot disk manifest onto local NVMe, **pins it**, and gates image readiness on
  it — the scheduler only places a session on a host once every base disk chunk is
  resident. Prod metrics on the dev-brain fleet confirm it: only `chunk_cache_hits{tier=nvme}`,
  **no `blobstorage`/miss/`refetch_after_evict` series at all** (lazily-registered counters
  that never fired) → 0 GCS misses and 0 mid-session re-fetch at read time.
- **Consequence:** a disk **working-set trace to narrow a GCS prefetch has nothing to
  narrow** (dropped), **readahead is marginal** (NVMe + the guest kernel already does
  block-device readahead, dropped), and **pinning the per-session set is moot** (0
  refetch, dropped). What remains is that the serve loop serializes reads *even though the
  chunks are local* — paying one daemon round-trip per request and one chunk fetch at a
  time. That shows up in the dominant cold-restore phase (`agent_handshake`) and, more
  acutely, caps throughput for in-session builds that read thousands of files.

**The invariant the serialization currently makes free:** the snapshot pipeline pauses the
guest, then calls `backend.wait_idle()` to drain the virtio→kernel-NBD→userspace pipeline
before flushing (ADR 0018). Today, with one in-flight op, "drain" is trivial. Any
concurrency design must keep `wait_idle()` correct: it must not return until every accepted
request's reply has been written.

## Decision

**1. Pipeline the serve loop** (`disk_daemon/runtime.rs::serve_loop`). Split the duplex
`UnixStream` with `into_split()`:
- A **reader** parses each 28-byte header; `Disconnect` ends the loop; a `Write` reads its
  trailing payload inline (it must stay ordered on the read half). For each request it takes
  an `InFlightTracker` guard, acquires a `Semaphore` permit (degree
  `ENGRAM_NBD_SERVE_CONCURRENCY`, conservative default, **`=1` reproduces today's exact
  serial behavior as a kill-switch**), and spawns a handler.
- A **handler** computes the reply bytes (`backend.read` / `backend.write`, or an ack for
  `Flush`/`Trim`) and sends `{framed reply, payload, guard, permit}` to the writer over a
  **bounded mpsc**.
- A single **writer** owns the write half and serializes every reply onto the wire. This is
  what keeps the wire well-formed: NBD replies are `handle`-correlated, so out-of-order
  *completion* is legal, but concurrent *writes* to one socket are not. The writer drops the
  in-flight guard **after** `write_all` flushes.

`serve_loop` runs the reader inline, then awaits the writer so all in-flight replies drain
before it returns. Because the guard is released only post-flush, `wait_idle()` keeps its
exact ADR 0018 meaning under concurrency. The kernel `NBD_SET_TIMEOUT` backstop is unchanged.

**2. Parallelize the per-read chunk fetch** (`backend.rs::read`). Replace the serial
`while cursor<end { read_chunk().await }` with an order-preserving concurrent fetch of the
spanned chunks (`buffered(N)`, bound `DISK_READ_FETCH_CONCURRENCY`), reassembled in order.
`read_chunk` takes `&self` and consults dirty/pending/mem-cache/base under brief locks, so
concurrent fetches are safe. Smaller win than (1) — most reads span one 16 MiB chunk — but
on the same path and cheap.

**Concurrent read/write safety:** the backend already serializes dirty-buffer mutations
under its lock, and the kernel does not issue dependent overlapping requests concurrently,
so concurrent handlers cannot observe torn state.

## Consequences

- The dominant restore phase and in-session build throughput improve by overlapping the
  per-request daemon round-trips and (cold-from-RAM) NVMe reads. The disk-page-in share of
  restore is bounded (the bulk of a resume is memory page-in + guest thaw), so the headline
  win is in-session build throughput — to be measured before/after.
- Reliability-critical surface: the serve-loop rewrite touches the path the snapshot drain
  depends on. Mitigated by (a) the guard-travels-with-the-reply design, (b) a
  `wait_idle()`-under-in-flight-load unit test, (c) the `=1` kill-switch, (d) an FC
  integration test wired into CI (snapshot-during-IO consistency).
- FC/Linux-only: the daemon is `#![cfg(target_os="linux")]`; VZ serves a file-backed rootfs
  and is unaffected. macOS clippy cannot see this code — validate the cfg(linux) paths with
  the `aarch64-unknown-linux-musl` clippy cross-check.

## Alternatives considered (and why not, now)

- **Disk working-set trace + restore prefault / pinning** (memory-substrate parity): moot —
  the rootfs is already NVMe-resident and pinned by image-prefetch; 0 measured GCS misses.
- **Sequential readahead in the daemon:** marginal — NVMe-resident, and the guest kernel
  already reads ahead on the block device.
- **io_uring for the chunk-file reads:** a legitimate later lever (the cache uses
  `tokio::fs`, a blocking-threadpool bounce), but diluted by the existing 64 MiB in-RAM
  chunk LRU, the 16 MiB chunk granularity (large, data/sha256-bound reads), and the fact
  that the serial serve loop — not syscall efficiency — is the dominant cost. `tokio-uring`
  also doesn't compose with the main tokio runtime. Deferred; if a cold-NVMe read tail shows
  up after this change, evaluate mmap / pre-sized reads first.

## Note

A filename collision already exists in `docs/adr/` (two `0060-*.md`). Flagged here; left for
a dedicated cleanup rather than touched in this change.
