# ADR 0093: Streaming packer — fuse flatten/pack/chunk via mkext4

Status: Accepted

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
- **Clean break — e2fsprogs retires everywhere** (owner directive at
  phase 2): no kill switch, no legacy path. `Mke2fsPacker`,
  `resolve_mke2fs`/`ENGRAM_MKE2FS`, the pinned static mke2fs shipped in
  the cli-tools artifact, `ensure-reproducible-mke2fs.sh`, and the
  nix/CI e2fsprogs installs are all deleted in this PR. The tree-based
  flatten engine (`flatten.rs`/`flatten/parallel.rs`) and
  `inject_init`'s tree write retire with them. Rollback is `git
  revert`, not an env var — the determinism cutover already forces
  re-capture either way, so a runtime toggle would buy nothing and
  cost a second maintained path.
- **mkext4 pinned `=0.0.3`**: 0.0.2's O(N²) flat-directory declare
  (linear duplicate scan) is fixed by 0.0.3's indexed lookup. The
  bump landed inside this PR so the determinism domain is minted
  once: 0.0.2→0.0.3 verified byte-identical on the full dev-brain
  image (same `ManifestRef`) before pinning.

## Gates

- Semantic: fixture layers (whiteouts, opaque, overwrite, hardlinks,
  hostile symlinks, zip-slip) → build → namespace/content assertions
  via the mkext4 verification reader (runs on macOS, no mounts). The
  legacy flatten's semantic test matrix ports over entry-for-entry.
- One-time legacy A/B (local, recorded at close): the same fixture
  layers through the pre-deletion flatten+mke2fs path and the
  streaming path → tree-content equality. Not a CI gate — mke2fs no
  longer exists in CI; the FC boot test is the standing kernel
  oracle.
- Determinism: streaming path double-run → identical `ManifestRef`.
- FC lane: boot a streaming-packed image in a real guest (wired into
  ci.yml's `--test` list).
- Sink: exactly-once coverage, out-of-order chunk completion, zero
  elision, byte-faithful manifest vs `chunk_file_into` on identical
  bytes.
- Perf: local dev-brain A/B via the mat-profile driver, recorded here
  at close.

## Close (2026-07-15)

All gates green. **Accepted 2026-07-15**: the first prod re-enable on
this build (enable job `ce067a91`, dev-brain) ran end to end:

| leg | pre-0093 baseline | prod on 0093 |
|---|---|---|
| pull | 2 s | 1.7 s |
| flatten → declare | 2m56s | 2m41s (GHCR-bound; ~15 s compute) |
| pack → seal | 1m48s | **1.3 s** |
| chunk → fill+upload | 2m09s | **1m26s** (full 14.2 GiB, zero dedup — the cutover run) |
| **materialize** | **6m55s** | **4m10s** |
| capture boot | 4 s | 3.0 s (NBD against the write-through-seeded NVMe cache) |
| seed dump | 1m48s | 1m47s |
| warm hook | 20m34s → **exit 1** | **9m05s → exit 0** |
| cold-base upload join | (open at failure) | 62 ms — fully hidden |
| final snapshot | — | 4m18s |

The two-bake-old "unfixable" warm-hook failure did not reproduce: with
capture co-located and every guest page-in served from the local NVMe
cache the streaming pack seeded, the in-guest brain stack boots inside
its own timeout. The hook bug was a starved guest wearing a script
error's clothes. First successful dev-brain enable since 2026-07-11;
enable-to-ready 41 min total, of which ~22 min is the post-capture
chunk prestage fan-out — the next latency lever, out of scope here.

**Measured — local dev-brain** (551,775 entries, 37 layers, 30 GiB
image, Apple Silicon; mat-profile driver, local blob store):

| leg | prod (#664 build) | streaming (local) |
|---|---|---|
| flatten / declare | 176 s | **12.6–17.1 s** (pipelined under pull) |
| pack / seal | 108 s | **0.6–0.7 s** |
| chunk / fill+upload | 129 s | **41–48 s** (fused; 907 chunks up, 885 zero-elided) |
| non-network compute | ~413 s | **~55–70 s** |

`partial_buffer_high_water` = 32 MiB — the dense-prefix emission claim
holds with ~64× headroom under the 2 GiB cap. Double-run reproduced
the identical `ManifestRef` (`24f0fb1e-…@v1`) with identical chunk
sets: content-derived identity holds on the full real namespace.
Legacy A/B: full semantic parity (names/kinds/modes/owners/mtimes/
targets/content/hardlink-groups) across whiteouts, opaque dirs,
usrmerge, setuid, multi-extent files; only normalizations were
unprivileged-run artifacts (sidecar ownership, macOS symlink umask).
`just check`: 1625/1625.

**Divergences found during implementation:**

1. **Symlink modes are forced 0o777** — the A/B caught the adapter
   honoring tar's symlink mode; Linux has no lchmod, so the tree the
   legacy pack read never did. (The one real bug the A/B existed to
   catch.)
2. **Declaration-order bootstrapping**: implied parent dirs must take
   their declaration sequence BEFORE the entry that implies them, or
   replay orders children ahead of parents.
3. **GNU-sparse tar entries** zero-pad to the declared size on a short
   read (warn-logged) — parity with the legacy engine, which wrote
   whatever the tar crate yielded.
4. `inject_init` survives (fixture-tree bakes pack via
   `stream_pack::pack_tree`, which walks the shim up); only the
   enable pipeline's tree write retired.
5. `nbd_chunked_disk`'s debugfs INSPECTION stays (distro-provided,
   graceful skip) — it post-hoc edits arbitrary pre-existing images,
   which is not a packer concern.

**Cross-repo follow-ups:** engrams-internal references the deleted
`setup-reproducible-mke2fs` composite action (companion cleanup
needed).

## Commit chain

`ADR 0093 (Proposed)` → `deps: pin mkext4 =0.0.2` → `chunk-store:
streaming region chunker` → `ADR 0093: clean break` → `materializer:
tar→mkext4 namespace adapter` → `materializer: wire the streaming
pack` → `gates: one-time legacy A/B` → `retire e2fsprogs/mke2fs
everywhere` → `unset the mke2fs-era knobs` → `ADR 0093 close (this
commit)`.

## Addendum (2026-07-20): `suggested_disk_gib` floors the packed ext4

The packer sized every image purely to content —
`recommended_size = max(2×content, content+128 MiB)` — which made the
image's `resources.suggested_disk_gib` a **dead knob** for workspace: it
fed host placement (`DiskLimit`, the 2D packing bound) but nothing ever
grew the filesystem (there is no in-guest resize path), so a slim image's
sessions got only content-relative headroom no matter what the resources
declared. Found live (ADR 0100): the PR-review finder on the 16-"GiB"
demo image died mid-clone of a large repo with `No space left on
device`, and bumping the setting to 100 changed nothing.

Now `suggested_disk_gib` ALSO floors the packed ext4 at enable-time
materialization: `MaterializeImageRequest.min_disk_gib` (wire v18)
carries the enable job's `image_config.resources.suggested_disk_gib` to
the host, and `NamespaceBuilder::seal` takes
`size = max(recommended_size(content), floor)`. The floor is ~free at
rest — padding is zero-filled and zero chunks are elided from manifests
(a 16 GiB fs over 3 GiB of content stores ~3 GiB of chunks) — and
deterministic (size is a pure function of content + config). Costs that
do scale with the floor: enable-time chunk hashing walks the full fs
size, and restored-session writes into padding become real chunks like
any other write. Changing the value changes the disk manifest, so a bump
takes effect on the next refresh/enable (re-capture), never on live
sessions — and mixed-roll, a v17 host ignores the field and packs
content-sized (hence the explicit wire bump).
