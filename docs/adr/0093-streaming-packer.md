# ADR 0093: Streaming packer — fuse flatten/pack/chunk via mkext4

Status: Proposed

## Context

After the enable-latency overhaul (ADR 0088 addendum, PRs #663/#664), a
dev-brain materialize is ~7 minutes: pull 2s (pipelined), flatten 176s,
pack 108s, chunk 129s. The remaining cost is structural, not
incidental: the rootfs content is written to disk as a directory tree
(flatten), re-read and re-written as an ext4 image (`mke2fs -d`), then
re-read a third time to chunk and upload. Three full passes over
~28 GiB, two of them existing only to feed the next stage.

The ADR 0088 addendum recorded the fix as a future direction: a
deterministic **streaming packer** that builds the ext4 image directly
from the layer streams and hands finalized byte ranges straight to the
chunk uploader. That crate now exists: **`mkext4`** (pure-Rust
deterministic ext4 writer, github.com/cortexapps/mkext4), designed for
exactly this consumer. Its two load-bearing properties:

- **All metadata bytes are a pure function of (namespace, sizes,
  options)** — ext4 has no data-block checksums — so the complete
  metadata image is emitted the moment the namespace is sealed, before
  any file content is supplied.
- **Sink contract**: every byte of `[0, image_len)` is emitted exactly
  once (as `data` or `zeros`) and is final on emission. Data blocks
  land in declaration order at ascending offsets.

## Decision

Replace the enable materializer's flatten → inject → pack → chunk
stages with a **two-pass, file-less pipeline** over the OCI layers:

1. **Declare pass** (tar headers only, layer order): apply OCI
   namespace semantics — sanitize/zip-slip, symlink-scoped resolution
   (usrmerge), whiteouts, opaque dirs, overwrite-wins
   (remove + redeclare), hardlinks — as `FsBuilder` declarations
   against an in-memory path→handle model. Sizes come from tar
   headers. The init shim is declared here too (retires
   `inject_init`'s tree write). Compressed layer files stay on
   scratch.
2. **Seal**: freeze the layout with the existing determinism inputs
   (`DETERMINISTIC_FS_UUID`, hash seed, epoch 1704067200,
   `recommended_size` banding, `inode_count_for` quantizer).
   `layout.writer(sink)` immediately emits every metadata byte and
   every zero run.
3. **Fill pass** (layer order again): re-decompress each layer and
   `fill` each *surviving* entry's bytes. Declaration order = layer
   order, so fills stream at ascending offsets.

The sink is a **streaming chunker** in `engram-chunk-store`: it
accumulates 16 MiB-aligned ranges, and the moment a chunk's byte range
is fully covered it hashes and uploads it (64-wide, `UploadBudget`-
aware, dedup HEAD as today). All-zero chunks are elided from the
manifest exactly as `chunk_file_into` does. **No `rootfs.ext4` file is
written at all**: `put_chunk` already write-throughs the host-local
NVMe cache (ADR 0078 move 4), which is what the co-located capture
host's NBD page-in reads — the image file had no consumer after
chunking.

Net effect: one decompress-and-write pass over content instead of
three passes plus a tree; pack and chunk cease to exist as separate
stages. Wire-stage vocabulary stays the 4-stage set (mid-roll skew
constraint, ADR 0088 addendum): declare reports as `flatten`, seal+fill
report as `pack` then `chunk` by fill progress.

## Consequences

- **One-time chunk-cache invalidation.** mkext4's bytes differ from
  mke2fs's (same semantics, different layout policy): the first
  re-enable of every image re-uploads its full chunk set and forces a
  fresh base capture. Accepted as a clean break — same cost as any
  determinism-affecting change; dedup re-establishes from the next
  bake onward. `mkext4` is pinned `=exact` version; a version bump is
  a deliberate fleet-wide invalidation event, never a float.
- **Fidelity improvement**: device nodes / FIFOs, which the tree-based
  flatten had to skip (`mknod` needs root), are now declared into the
  image. `skipped_specials` reporting retires.
- **Kill switch**: `ENGRAM_STREAMING_PACKER=0` reverts to the legacy
  flatten+mke2fs path, which stays in-tree until this ADR flips to
  Accepted on prod evidence; its removal is the closing commit.
- **mkext4 0.0.2 caveat**: declaration is O(N²) in single-directory
  entry count (linear duplicate scan). Nested trees (node_modules
  shape) are unaffected; a pathological flat directory would regress
  declare. 0.0.3 fixes this (indexed lookup, in review); we bump the
  pin — accepting the byte-layout consequences if any golden churns —
  before enabling by default in prod.

## Gates

- Differential: same fixture layers through legacy and streaming
  paths → namespace/content equality via the mkext4 verification
  reader over both images (runs on macOS, no mounts).
- Determinism: streaming path double-run → identical `ManifestRef`.
- FC lane: boot a streaming-packed image in a real guest (wired into
  ci.yml's `--test` list).
- Sink: exactly-once coverage, out-of-order chunk completion, zero
  elision, byte-faithful manifest vs `chunk_file_into` on identical
  bytes.
- Perf: local dev-brain A/B via the mat-profile driver, recorded here
  at close.

## Commit chain

(recorded at close)
