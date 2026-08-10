# ADR 0035: Content-addressed bundle generations, pinned by GC

Status: 2026-06-03 — **Accepted.** Implemented on PR #72; CI-validated
end-to-end: the e2e stack job runs the full enable → capture → session
chain against content-addressed staging (resolve via stamp → attach →
init-shim mount → agentd activate → in-session assert), and the FC
integration suite pins the pinned-generation reopen, the **incident
reproduction**, and the §3 load-paused `patch_drive` swap on real
FC/KVM. Remaining validation (tracked, non-blocking): a dev-vm e2e of
the swap + agentd squashfs-remount combination (the e2e runs with
`pinned == current`, so the swap path's guest-mount half is pinned at
the block layer only), plus the prod rollout itself — merge auto-rolls
coord + host MIG, then the active images must be **re-enabled** to
capture generation-pinned base snapshots (prod sessions stay
bundle-broken from the incident until that step).

ADR 0027's RO-mount bundle engine ships the fleet-wide `skills` /
`playwright` squashfs bundles at a **fixed canonical path**
(`/var/lib/engram/shared/<name>.squashfs`) and re-anchors snapshots against
that path "by mere presence". That contract has a hole we hit in production
on 2026-06-03: presence is asserted, **content identity is not**. This ADR
replaces the fixed path with **content-addressed bundle generations**
(`<name>-<sha256>.squashfs`) whose lifetime is governed by the GC's pin set
— a bundle generation exists exactly as long as a live snapshot references
it. No fixed path, no "keep N", no deploy-ordering rules.

## The incident (why presence isn't enough)

Timeline, 2026-06-03 (UTC):

1. `05:01` — `dev-engrams:warm-3a91b53` base snapshot captured. The capture
   VM boots with the skills bundle attached as a virtio-blk drive and
   **mounts it**; the squashfs superblock + partially-populated page cache
   are frozen into the memory snapshot.
2. `da00edf` pushed to main — a SKILL.md edit. Per the auto-deploy triggers,
   this re-baked the FC-host image (the bundle bytes changed) and **rolled
   the MIG at 14:03**. New hosts carry a *different*
   `/var/lib/engram/shared/skills.squashfs` at the same path.
3. `15:13` — a session restores the 05:01 snapshot on a 14:03 host. FC's
   `load_snapshot` reopens the drive at the embedded path; the restore-side
   presence assert passes. The guest's in-memory squashfs superblock now
   reads blocks from a file it never mounted → **EIO on every read of
   `/opt/engram/skills`**.
4. `engram-session-bundles::activate()` is best-effort by design: its
   `.exists()` probe hit EIO → `skills_mounted = false` → **no
   `/etc/gitconfig` credential block, no `engram-pr`, no bundle skills at
   all** — in every session, fleet-wide. First visible symptom: `git fetch`
   dying on `could not read Username for 'https://github.com'` (the repo is
   private; the askpass wiring was silently gone). Push was equally broken.

The capture-side hard-fail ("roll the host image first, THEN enable")
guards exactly one ordering. Nothing invalidates **already-captured**
snapshots when a later push mutates the bundle at its fixed path — and
*every* bundle edit does, because the path never changes. The ADR 0027
comment "present identically on every host" was an assumption, not an
invariant. The poetic kicker: the SKILL change that broke the fleet was the
one telling agents to `git fetch` first.

## Decision

Five moves, two invariants.

**Invariant 1 (identity):** a snapshot's `state.bin` only ever references
bundle paths whose *content* is immutable — the sha256 is in the filename —
and a referenced generation is **pinned**: it cannot disappear from blob
storage or from a host's staging dir while any live snapshot points at it.

**Invariant 2 (freshness):** pinning is for *restore correctness*, never a
freshness ceiling. A fresh session create always attaches the host's
**current** generation; ADR 0027's "a skill edit ships fleet-wide without
any per-image re-bake" is preserved — the delivery latency is one host
roll (which the bundle edit auto-triggers), not an image re-enable.

### 1. Content-addressed staging

The FC-host image bake stages bundles at
`/var/lib/engram/shared/<name>-<sha256-hex>.squashfs` (full 64-char hex; the
hash is computed by the Packer provisioner from the pulled artifact, so the
engrams-internal bake workflow doesn't change). The bake also writes a stamp,
`/var/lib/engram/shared/current.json`:

```json
{ "skills": "<sha256>", "playwright": "<sha256>" }
```

The fixed `<name>.squashfs` path is **gone** — no symlink, no fallback.
Nothing may reference a mutable path. (Zero-users clean break; every
existing snapshot is already broken by the incident, so there is nothing to
stay compatible with. Re-enabling the active images after this rolls is the
remediation anyway.)

### 2. Capture resolves "current", records identity, publishes

`SandboxSpec.aux_ro_drives` from the coord stays **symbolic** — drive_id +
guest mount + fs type, same as today's constructors; the coord doesn't know
hashes. At `build_base_snapshot` the FC backend resolves each drive against
the host's `current.json` stamp:

- stamps `sha256` on the drive entry — the host path is *derived* from
  `(drive_id, sha256)`, so path and content can never disagree; this
  resolved form is what the sandbox manifest (and FC's `state.bin`) embeds;
- **publishes** the bundle bytes to BlobStorage at `bundles/sha256/<sha>`
  iff absent (idempotent HEAD-then-put; hosts already hold a BlobStorage
  handle). Publish-on-first-reference means blob storage only ever holds
  generations some snapshot referenced.

`SnapshotMetadata` gains `aux_bundles: Vec<BundleRef>` (`drive_id`,
`sha256`, `size_bytes`), filled by the host from the resolved spec — for
the base capture **and** for every eviction snapshot (an evicted VM's
device model still points at the same generation, so the pin must ride
every snapshot row in its lineage). The coord stamps it into a new
`snapshots.aux_bundles` jsonb column (migration 0051).

### 3. Fresh creates track current; resumes pin

The two restore flavors have different correctness constraints, so they
attach differently:

- **`restore_base_for_session` (fresh create, incl. warm-pool refill):**
  at base-snapshot capture nothing in the guest holds the bundle open —
  capture is boot-to-agentd-ready, before any harness bind; the squashfs
  is mounted but has zero open fds. So after `load_snapshot` (paused),
  the host `patch_drive`s each aux drive whose pinned generation differs
  from the host's current one — the proven ADR 0014 option-D mechanism
  (`patch_drive_swap`) — and agentd **umount/remounts** the bundle mounts
  at session bind, before `activate()`, so the guest's superblock matches
  the swapped device. New sessions therefore always run the latest
  skills; no image re-enable needed.
- **`restore` (resume of an evicted session):** live guest processes may
  hold fds *into* the bundle (playwright runs *from* it; a skill file may
  be mid-read). Swapping under them is exactly the incident's corruption,
  and the umount would EBUSY anyway. Resumes re-attach the **pinned**
  generation from the snapshot — an in-flight session keeps the world it
  was working in until it dies.

agentd's bind-time remount is best-effort with an asymmetry that makes it
self-consistent: on the fresh-create path there are no fd holders, so the
umount succeeds and the remount picks up the swapped device; on the
resume path the umount may EBUSY precisely because processes are using
the (unswapped, still-correct) mount — log and continue.

The pinned generation file must still be present even on the swap path:
FC's `load_snapshot` opens the `state.bin`-embedded path *before* any
`patch_drive`. That — plus resumes — is what the pin set protects.

### 4. Restore fetches a missing generation

The restore-side check stops being an assert and becomes a **materialize**:
if `<name>-<sha>.squashfs` is absent on the receiving host, fetch
`bundles/sha256/<sha>` from BlobStorage to a temp file, verify the digest,
and atomically rename into place. This is the rare path — the common case
is a heartbeat-driven prefetch (below) or the generation being the one
baked into the host image. A fetch failure fails the restore loudly, same
as today's assert, but now only when blob storage itself lost the bytes
(GC bug), not whenever a deploy raced an enable.

The hash needed for verification travels on the drive entry itself
(`AuxRoDrive.sha256`), recorded at capture.

### 5. GC pins; nothing else does

Liveness for bundle generation `<sha>` = **referenced by `aux_bundles` of
any `snapshots` row**. Retention is exactly the pin set — no keep-N, no
age-based policy:

- **Blob storage:** a `bundle_gc` pass rides the existing GC sweep loop
  (`gc_sweep_loop`), reusing the `chunk_generation` barrier (it already
  ticks on `record_snapshot` / enable — precisely the events that pin
  bundles) and the candidates-with-grace pattern: list
  `bundles/sha256/`, diff against the pin set, upsert unpinned keys into
  `bundle_gc_candidates` (migration 0051), promote-delete after the same
  24h grace.
- **Host staging dirs:** the heartbeat ack gains
  `live_bundles: Vec<BundleRef>` (the coord's pin set, plus implicit
  protection for the host's own `current.json` generations). The
  host-agent's supervisor (a) prefetches any live generation it's missing
  — so restores land warm — and (b) deletes staged
  `<name>-<sha>.squashfs` files that are neither live nor current. The
  bundle path is thereby "pinned by GC" on hosts too: stage-by-bake,
  retain-by-reference.

Heartbeats additionally report the host's `current.json` map
(`current_bundles`), which (a) lets operators see fleet skew mid-roll and
(b) defensively joins the blob-GC pin set so a freshly-baked generation
can't be collected in the window between MIG roll and first capture.

## What deliberately doesn't change

- **The guest contract, almost.** Mounts at `/opt/engram/skills` /
  `/opt/engram/browser`, agentd's `activate()` gating, the init shim — all
  unchanged; the guest never sees a host path or hash. The one addition is
  agentd's best-effort bind-time umount/remount of the bundle mounts (§3),
  which is what makes the fresh-create drive swap visible to the guest.
- **Bundle build + publish to GHCR.** `deploy/bundles/*/build.sh` and the
  OSS `publish-bundles` CI job are already content-correct (they hash the
  artifact); only the host-side staging name changes.
- **`enabled_images` capture flow.** Still symbolic `AuxRoDrive::skills()` /
  `::playwright()`; resolution is the host's job (the host owns the staged
  files — detection above the cfg-gated leaf, switch at the layer that has
  the facts).
- **ProcessBackend (dev).** Stages bundles from `var/bundles/<name>/` dirs
  and ignores `spec.aux_ro_drives`; unaffected.
- **VZ.** Doesn't attach aux drives today (`snapshot.rs` builds
  `aux_ro_drives: Vec::new()`); the parity gap predates this ADR and is
  tracked separately. FC is production; this ADR fixes production.

## Consequences

- A bundle edit merges → host image re-bakes → MIG rolls → **existing
  snapshots keep restoring against their exact pinned generation**, old
  hosts keep working, and the next image enable / re-enable picks up the
  new generation. The deploy-ordering footgun ("roll first, then enable")
  disappears, along with the capture-side hard-fail it justified.
- Blob storage grows by one squashfs per *referenced* generation
  (~de-duplicated by content; skills is ~1 MB, playwright ~300 MB) and
  shrinks as snapshots are deleted — same lifecycle as chunks.
- First restore of an old snapshot on a freshly-rolled host may pay a
  one-time bundle fetch if prefetch hasn't landed; subsequent restores hit
  the staged file. The prefetch supervisor makes this rare.
- One migration (0051): `snapshots.aux_bundles jsonb NOT NULL DEFAULT '[]'`
  + `bundle_gc_candidates`.

## Divergences found during implementation

- **`path_on_host` is gone entirely, not rewritten.** `AuxRoDrive` now
  carries `sha256: Option<String>` and the host path is *derived* from
  `(drive_id, sha256)` — path and content structurally cannot disagree.
  Symbolic (`None`, coord request) vs resolved (`Some`, recorded
  manifest) replaced the planned in-place path rewrite.
- **Eviction snapshots publish too.** A fresh-create swap pins the
  host's *baked* generation, which no base capture ever published —
  so `BundleStore::publish` runs idempotently on **every** snapshot
  with aux refs, not just base captures.
- **Hosts' current generations don't join the blob-GC pin set.**
  Publish-on-first-reference means an unreferenced current generation
  isn't in blob storage at all; the host-side sweep protects its own
  stamp locally. The heartbeat's `current_bundles` is therefore pure
  fleet-skew visibility (kept on `HostState`).
- **Bundle GC has no knobs of its own.** It rides the chunk-GC loop,
  the `chunk_generation` barrier (which already ticks on
  `record_snapshot` — exactly when bundle pins change), and
  `ChunkGcConfig`'s grace/batch settings. Admin mirrors:
  `POST /api/admin/bundle-gc/{dry-run,sweep}`.
- **`live_bundles` is deliberately not `serde(default)`** on the host's
  ack decode: an old coord's ack (mid-deploy) must fail decode — and
  the heartbeat retry it — rather than read as an *empty pin set*,
  which is an instruction to sweep generations resumes still need. A
  coord-side PG failure likewise fails the heartbeat instead of
  degrading to an empty set. Pinned by a wire-contract test.
- **The swap mutates the live spec.** After a fresh-create
  `patch_drive`, the restored sandbox's in-memory spec records the
  *current* sha, so a later eviction `snapshot()` pins what's actually
  attached (and the swept test kernel can't mount squashfs, so the FC
  integration tests pin the block layer; the agentd umount/remount
  half is covered by dev-vm e2e).
- **`tests/aux_ro_drive.rs`'s symlink-roll variant was the incident,
  encoded as a feature** ("the guest sees the NEW bytes — this is why
  we embed the stable symlink"). It's replaced by a pinned-reopen
  test, an explicit incident-reproduction negative control, and a §3
  swap test — and the file is now wired into ci.yml's FC test list
  (it was dev-vm-only).

## Commit chain

All on PR #72 (`adr-0035-content-addressed-bundles`):

- `e3b3202` — ADR authored (Proposed): incident root-cause, the two
  invariants, GC-pinned retention design.
- `d11d0d6` — core + FC: `AuxRoDrive` drops `path_on_host` (path
  derives from `drive_id` + `sha256`; symbolic vs resolved),
  `SnapshotMetadata.aux_bundles`, FC resolve-at-capture /
  pin-assert-at-restore / `restore_fresh` load-paused swap.
- `70a720d` — host: agentd bind-time umount/remount (EBUSY = resume,
  kept), `BundleStore` publish/materialize/sweep, pooled
  resume-vs-fresh restore split, heartbeat stamp + `live_bundles`
  supervisor (hard-decode anti-sweep contract).
- `4401f55` — coord: migration 0051 (`snapshots.aux_bundles` +
  `bundle_gc_candidates`), pin stamping on every snapshot flavor,
  pin-set heartbeat acks (PG failure fails the heartbeat), bundle GC
  riding the chunk-GC loop/barrier/grace, admin
  `/admin/bundle-gc/{dry-run,sweep}`.
- `f44491f` — staging: packer bakes `<name>-<sha>.squashfs` +
  `current.json` (engrams-internal workflow untouched);
  `just bundles-squashfs` dev mirror; README/init-shim docs.
- `90df3c9` — tests: `aux_ro_drive.rs` rewrite (the old symlink-roll
  variant had pinned the incident as a feature; replaced by pinned
  reopen + incident negative control + §3 swap, newly wired into
  ci.yml), `bundle_gc_live_pg`, `BundleStore` units, heartbeat wire
  contract, FC stamp-resolution units.
- `8c72579` — divergences recorded (this section's sibling above).
- `aeddad8` — the e2e job's own bundle staging migrated to the
  content-addressed shape — its 500 on the first CI run was the new
  resolve step refusing a stamp-less host, i.e. the loud-failure
  behavior working as specified.

## Amendment (2026-08-10): durable-at-birth generations + the complete pin set

The 2026-08-10 `chain_poisoned` firing exposed two holes in this ADR's
retention model. Session `e83383b2`, sandbox `768b22e7`:

1. A fresh create swapped the VM's `dyn_0` drive to the host's current
   stamp generation (§3).
2. A deploy rolled the host-agent DaemonSet; the new pod's init staged a
   new stamp and the swapped-in generation rotated out of `current.json`.
3. The new pod's supervisor swept the node's only copy — the sweep
   keep-set is `live_bundles` ∪ the boot-time stamp, and a RUNNING
   sandbox's attachment is in neither.
4. The generation had never reached blob storage: publish ran only at
   snapshot time, and no snapshot had referenced it yet.
5. The VM's first Diff capture could not publish its pinned generation
   (HEAD miss + staged file gone) *after* FC had consumed the KVM dirty
   bitmap, so the checkpoint chain was poisoned. The gate held; no
   corrupt data was served.

The two structural defects, and the two decisions that close them:

**Durability was circular** — a stamp generation became durable only via
the first snapshot that pinned it, but that snapshot needs the bytes to
still exist. Between create and first checkpoint the bytes were
single-copy on node-local disk. The catalog producers never had this
defect (`RegisterSkill` / `RegisterHarness` upload before writing the
row); the baked stamp path (bake → GHCR → node-assets image → hostPath)
was the one producer class that skipped the upload.

**D1 — publish stamp generations at host-agent startup.** After
`read_stamp`, a background task runs the idempotent HEAD-first
`BundleStore::publish` over the stamp's generations, retried on the
supervisor's 60 s cadence until one pass succeeds. It never gates host
readiness (a blob outage must not stop the host serving); the
snapshot-time publish stays as the load-bearing verify-and-backstop, now
a HEAD hit in the common case. The publish point is the host, not CI:
the OSS bake publishes to GHCR only and holds no deployment blob
credentials. N hosts staging the same bake race at N HEADs and at most
one PUT per generation. Failures increment
`engram_bundle_startup_publish_failures_total` (pre-registered).

**The pin set was incomplete** — `bundle_pin_set` covered snapshot rows
∪ the catalogs. A generation attached to a running sandbox, or staged as
a live host's current stamp, pinned nothing, so both the host sweep and
the bundle GC could reclaim bytes a live VM's next snapshot needed.

**D2 — pin live-sandbox attachments and host stamps.** A new
`SandboxBackend::aux_bundles_all` (default empty) reports each running
sandbox's attached generations from the backend's live view — post-swap,
and rebuilt from the persisted manifest on pidfd-reattach, so a roll
survivor reports the generations it really has open. The heartbeat
carries the report (`serde(default)` both directions; an old host
mid-roll reports nothing, covered by the GC's 24 h grace — no wire-version
bump, same posture as `current_bundles`). The coordinator persists it on
the hosts row (`sandbox_bundles` jsonb, migration 0113) and
`bundle_pin_set` gains two legs: per-sandbox attachments and current
stamps, both over `ready|draining` hosts. Ordering is load-bearing: the
heartbeat handler persists the report **before** computing the ack's
`live_bundles`, so a freshly rolled host's first ack already pins its own
reattached sandboxes — the exact window the incident rode.
`run_one_bundle_sweep` is unchanged; a complete pin set makes it correct
as-is.

The amended invariant: **a bundle generation that anything can reference
is durable in blob storage, and every live referent is visible to the
pin set.**

**Simulation (landed as the capstone).** The DST worlds could not
represent this incident class (every sim hardcoded empty `aux_bundles`,
no host sweep actor existed, and the cosim's coordinator and host used
two disjoint blob buckets). The capstone extends `engram-dst-cosim`:
ONE shared bucket backs the coordinator's blob tier, the host's chunk
store, the bundle store, and the bundle GC (optionally fault-wrapped by
`engram_testkit::FaultyBlobStorage` for `bundles/`-prefix put faults);
the cosim host carries a real staged-bundle dir + stamp and runs the
REAL `BundleStore` publish/materialize/sweep and the REAL
`run_one_bundle_sweep` at ZERO grace; sandboxes attach the current
stamp at create and every capture pins `aux_bundles` through the real
finalize; new swarm steps `HostHeartbeat` / `RollBundleStamp` /
`BundleGcSweep` compose the incident's interleaving. The standing
oracle, checked after every swarm step: *every generation attached to a
live sandbox is reachable from local staging ∪ blob storage, and every
recoverable snapshot row's pins are durable (publish-before-record)*.
The directed pre-fix replay (`publish=false` roll + empty-ack sweep)
fires the oracle and fails the next checkpoint — the detection this
incident lacked. The ADR 0098 D4 conformance obligations for the
`bundle_pin_set` change landed with D2 itself (`t_bundle_pin_set_union`,
the first conformance case for this method).

**The capstone's first catch (before any production firing):** the
bundle GC's promote pass deleted expired candidates purely on age — no
pin-set re-check at delete time, and the mark pass never clears a
candidate whose sha is pinned again. A generation marked while
transiently unpinned (the mid-roll window before a heartbeat reports
the new attachment) and pinned again before the 24 h grace elapsed
would have been wrongly deleted, 404ing the pinning restore. The chunk
GC and snapshot-blob GC both carry this re-check; the bundle sweep
alone lacked it. Fixed alongside the capstone
(`promote_repinned_skips` on the report, mirroring the siblings; six
swarm seeds reproduced the deletion deterministically).

Landed on PRs #1154 (D1), #1157 (D2), and the cosim capstone PR.
