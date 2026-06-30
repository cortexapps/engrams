# ADR 0062: Per-session harness selection via the RO-bundle engine

Status: 2026-06-30 — **Accepted** (per-harness bundle model; see the commit chain in §Phasing).
Builds on **ADR 0027** (the read-only host-mounted shared bundle engine), **ADR 0035**
(content-addressed bundle generations + the load-paused `patch_drive` swap), and **ADR 0055**
(dynamic per-session directory mounts — the reserved-slot pool this ADR reuses).
**Supersedes ADR 0021's retirement of per-session harness selection**: 0021 baked one harness
per image and deleted the standalone harness subsystem because its *delivery vehicle* (an
NBD/GCS harness substrate) was slow. That constraint no longer holds — the RO-bundle engine
delivers a harness from local NVMe with no GCS on the boot path — so we re-separate the harness
from the image and choose it per session. The control-plane / descriptor half is **ADR 0063**.

## Context — what changed since ADR 0021

ADR 0021 (§Context, the prod waterfall) measured the cost that drove the consolidation: the
**harness substrate bind** was ~24 *serial* `chunk.fetch @ ~83 ms` (GCS RTT) during
`fc.spawn_harness` — ~2 s of cold latency to assemble a per-session ext4 harness substrate over
NBD. The locked decision (2026-05-27) was therefore:

> one harness per image, baked at bake time (or none) … the per-session harness *selection* is
> retired — which harness an image runs is an image property.

That was the right call **for the delivery mechanism that existed at the time**. The harness now
lives in the rootfs at `/opt/engram/harness/`, the standalone subsystem (`harness_packs`,
`HarnessSpec`, `engram harness add/push/list/rm`, the NBD substrate) is gone, and
`resolve_harness` reads the image manifest's `[harness]` block.

Two things have since changed the calculus:

1. **The RO-bundle engine (ADR 0027 → 0035 → 0055) is a fast, per-session delivery vehicle that
   did not exist in 0021.** Skills are fleet-staged content-addressed squashfs at
   `/var/lib/engram/shared/<sha>.squashfs` on every host's local NVMe, attached as read-only
   virtio-blk on a **reserved 12-slot pool** captured into the base snapshot as sentinels, and
   `patch_drive`-swapped per session **in the already-existing paused restore window**
   (`engram-sandbox-firecracker/src/lib.rs`, the `load_snapshot` paused + `fc.swap_aux_bundles`
   block). No GCS on the boot path; no new round trip — the swap rides a window every skilled
   session already opens.

2. **Warm snapshots — 0021's other reason for one-harness-per-template — are closed, not
   pending.** ADR 0021's P3 status: "the substrate fix collapsed the agent cold start to ~5 s …
   and the warm-snapshot-shared model fights claude's env-at-exec secret injection. **P3 is
   closed.**" There is no warm VM pool (ADR 0055 §"The hard constraint"). So the coherence
   argument for binding a single harness into the template no longer has a live consumer.

Meanwhile the product wants what 0021 took away: **choose the harness per session.** Use Claude
Code for heavy planning, OpenCode + a small model for cheap task execution — same repo/image,
different agent driver. A profile should carry a *default* harness, overridable at session
create.

### The hard constraint (unchanged): do not inflate cold boot

Boot latency is non-negotiable (ADR 0019/0020/0055). The design rule we inherit from ADR 0055
applies verbatim, with the harness substituted for skills:

> **The base snapshot stays harness-agnostic. The selected harness binds in the per-session
> paused window after restore — never baked into captured state.**

This is not merely *safe* for cold boot; research shows it is **neutral-to-faster** than the
status quo. Today the harness binary lives in the image rootfs and pages in, on first exec at
`SpawnHarness`, from the chunked rootfs served over NBD (local-NVMe-resident, but through the
NBD/chunk indirection layer). A fleet-staged squashfs is (a) the *same* local NVMe tier, (b) a
plain file Firecracker opens directly as a virtio-blk backing — no NBD daemon, no chunk index,
(c) zstd-compressed so fewer disk bytes + in-kernel decompress, and (d) removing the harness
from the rootfs *shrinks* the base-snapshot disk manifest, marginally trimming rootfs page-in
and image materialize.

## Decision

**A harness is a read-only squashfs the coordinator mounts on a reserved slot (`dyn_0`) and the
guest `exec`s — exactly like a skill bundle, except it is `exec`'d (so the coordinator names its
guest path) rather than guest-activated.** Selection per session is "which squashfs mounts on
`dyn_0`." There are two delivery paths, both already proven by skills:

- **Built-in harnesses** (e.g. `claude`) ride the fleet **`current_bundles` stamp** — baked into
  the FC-host image as a content-addressed squashfs via node-assets, exactly like the built-in
  skills bundle — and their descriptor (`harness.toml`) is **embedded in the coordinator binary**.
  No registration, no admin step: a fresh deployment runs `claude` the moment the host image
  carries the stamp and the coordinator carries the descriptor.
- **Custom harnesses** ride the **`harness_catalog`** — registered as an OCI artifact, packed into
  a *single*-harness content-addressed squashfs in BlobStorage, and materialized to the host on
  demand — exactly like an *uploaded* skill (ADR 0055 P2 `mount_catalog`). The descriptor lives on
  the catalog row.

There is **no packed "catalog generation" and no all-harnesses-in-one-squashfs drive.** Each
harness is its own squashfs; `dyn_0` carries the *one* the session selected.

### 1. `dyn_0` = the selected harness squashfs, `exec`'d by argv

The harness rides the ADR 0055 reserved-slot pool (`AuxRoDrive::RESERVED_SLOTS = 12`,
`dyn_0..dyn_11` at `/opt/engram/dyn/<i>`), but unlike a skill it is **`exec`'d, not
`activate`d**: a skill is content-discovered guest-side (`engram_session_bundles::activate()`
reads each slot's `mount.json`), so its slot index is irrelevant to the coordinator. The harness
supplies `argv[0]`, which the coordinator must name *before* the guest mounts anything — so it
needs a deterministic, coordinator-known slot. We **reserve slot 0 (`dyn_0`)** for the selected
harness; skills assign to `dyn_1..dyn_11`. The harness squashfs is a single self-contained tree
(entry binary + sidecar runtime/CLI); the session execs `argv[0] = /opt/engram/dyn/0/<exec>` and
the wrapper self-locates its sidecars from `$0`.

`activate()` gains a one-line guard to **skip `kind=="harness"`** (it already skips
`kind=="sentinel"`) so the exec'd harness mount is never wired as a skill. `remount_bundle_mounts()`
already re-parses all of `/opt/engram/dyn/` **before** the harness exec, which is what makes
exec'ing from a freshly `patch_drive`'d squashfs correct (the captured superblock predates the
swap — the same 2026-06-03 incident-class guard skills rely on). **No init-shim or remount change**
is needed. Total aux-drive count stays 12 (no virtio-mmio GSI pressure, no device-count re-bake);
the cost is one skill slot — **agent sessions get 11 skills** (size-aware packing is the
documented fallback if a profile ever needs more).

### 2. Capture stays harness-agnostic — already true at the process level

`cold_boot_spec` already attaches all 12 reserved slots as the **sentinel** generation
(`engram-coordinator/src/api/sessions.rs`), and base capture waits only for **agentd-ready**,
never for a harness — `build_base_snapshot` snapshots a cold VM with *no* harness process (the
harness only ever exec's at the post-restore `SpawnHarness` RPC). So `dyn_0` is sentinel at
capture with **no capture-path code change**: the base snapshot is harness-agnostic by
construction, one per image. The single upstream change is that the rootfs no longer carries any
harness binary (§4), shrinking the base disk.

### 3. Per-session resolution + mount

The selected harness is "just another selected mount" bound on `dyn_0` through the unchanged
host/guest swap path:

- **Wire:** `CreateSessionRequest.harness` (`optional string`) carries the per-session harness
  name. The orchestrator supplies the profile default or the per-session override (ADR 0063); it
  threads identically to `selected_skills`.
- **`resolve_harness`** takes a harness *name* and resolves it to `(descriptor, sha256)` —
  mirroring `resolve_selected_skills`'s "fleet stamp ∪ catalog" lookup:
  - the **descriptor** (`harness.toml` → `exec`/`args` launch contract + the ADR 0063
    orchestrator-facing env contract) from the **built-in registry** (embedded) ∪ the
    **`harness_catalog`** row;
  - the **squashfs sha** from the fleet **`current_bundles` stamp** (built-in, via
    `fleet_bundle_catalog`) ∪ the **`harness_catalog`** row's content hash (custom);
  and emits `argv[0] = /opt/engram/dyn/0/<exec>` (with the backend dial flags
  `--connect`/`--vsock-host` + the descriptor's `args`) plus an
  `AuxRoDrive { drive_id: dyn_0, guest_mount: /opt/engram/dyn/0, fs_type: "squashfs",
  sha256: Some(<that harness's sha>) }` merged into `selected_mounts`.
- **`prepare_inner`** stops reading `manifest.harness`; skill-slot assignment starts at `dyn_1`.
- An agent-mode session whose harness name resolves **nowhere** (neither built-in nor catalog) is
  a hard create-time error, never a silent sentinel boot that hangs at `SpawnHarness`. `dev_vm`
  mode resolves to no harness (`dyn_0` stays sentinel — no harness is mounted for a pure dev VM).
- The host/guest plumbing (`restore_base_for_session` → `restore_in_jail` paused-window
  `patch_drive`, and the host-side **materialize-selected-mounts-from-blob** before
  `load_snapshot`) is the same path skills ride.

### 4. Built-in vs custom + the image clean break

| | Built-in (`claude`) | Custom |
|---|---|---|
| Squashfs | fleet `current_bundles` stamp (node-assets, baked into the host image) | `harness_catalog` row → single squashfs in blob → materialized on demand |
| Descriptor | embedded in the coordinator (`include_str!` the committed `harness.toml`) | stored on the catalog row, read from the OCI artifact at register |
| Registration | none — always present | `RegisterHarness(name, oci_ref)` |

The image no longer carries any harness. The `[harness]` block and the bake-time
harness-injection path are deleted, not dual-read (engram is pre-1.0, operator-curated; ADR 0021
§"No backwards compatibility" set the precedent):

- Drop `harness: Option<HarnessManifest>` from `ImageManifest`; delete `HarnessManifest` + its
  validators.
- Delete `engram-image-builder/src/harness.rs` (`inject_builtin_harness`, `BuiltinCatalog`,
  `Platform`, the canonical-path constants) and its call site.
- Remove the `ENGRAM_SESSION_HARNESS_NAME` spec-env injection (nothing consumes it) and
  `harness_name` from `image.proto` / the manifest lift; the create UI lists harnesses from the
  catalog read surface instead.
- Strip `[harness]` from demo `engram.toml`s and re-bake images harness-free.

### 5. The harness catalog (custom uploads only) — OCI in, one squashfs out

A `harness_catalog` table + `MetadataStore` methods (`register_harness`, `get_harness_by_name`,
`list_harnesses`, `soft_delete_harness`), mirroring `mount_catalog` / `CatalogSkill`. Each row:
`name → (oci_ref + resolved digest, descriptor TOML, single-harness squashfs sha)`. The catalog
holds **only custom uploads** — built-ins live in the stamp + the embedded registry and never
need a row.

**`RegisterHarness(name, oci_ref)`** (app-gRPC admin op, modeled on `RegisterSkill`):
1. Pull the OCI artifact (`engram_oci`), extract its tree, read + validate its `harness.toml`.
2. Pack that **one** harness tree into a content-addressed squashfs (the same reproducible
   `skill_pack`/`squashfs::pack_dir` machinery, `SOURCE_DATE_EPOCH=0`), publish it to
   `bundles/sha256/<sha>`, and store the sha on the row.
3. Upsert the catalog row.

**The read surface** (`ListHarnesses`, for the orchestrator/web pickers) returns the **embedded
built-ins ∪ the live catalog rows** — so `claude` appears without any registration.

**GC / staging** is the uploaded-skill path verbatim: a custom harness's sha enters the pin
universe through `snapshots.aux_bundles` (every live/evicted session pins the harness its `dyn_0`
references) **∪ every live `harness_catalog` row** (`bundle_pin_set()` unions them, exactly as it
unions `mount_catalog`). Hosts materialize a missing custom-harness squashfs from
`bundles/sha256/<sha>` via the bundle supervisor + the restore-path materialize. Built-in harness
squashfs are pre-staged in the host image (the stamp), so they never materialize. There is no
"generation" to pin or reclaim.

## Cold-boot latency — where time moves

Critical path at session **create** = base-snapshot **restore** (the ~15 s `await_agent_ready`
of ADR 0019 is *capture-time* cold boot, off the session hot path):

| Step | Today | Harness on `dyn_0` | Δ |
|---|---|---|---|
| `load_snapshot` (paused, UFFD) | ~5 ms | ~5 ms | 0 |
| paused-window `patch_drive` | N skills | N skills **+ 1 harness** | +1 `PATCH /drives` (µs), existing window, no new round trip |
| resume | — | — | 0 |
| `SpawnHarness` exec page-in (~0.8 s, ADR 0019) | harness faults from **rootfs over NBD** | harness faults from the local squashfs mount (zstd, direct virtio-blk) | **neutral-to-faster** |
| guest harness-slot remount | (skills only) | +1 squashfs remount | µs |

Mounting the harness squashfs is metadata-only (the superblock + root inode); the session pages
in only the harness binary it exec's. A built-in harness is pre-staged in the host image (no
materialize on the path); a custom harness materializes once per host from blob (the uploaded-skill
path), off the per-session hot path after the first session. Net: no GCS on the boot path, no new
round trips, smaller base disk. The one genuinely new behavior — **exec page-faulting from a
squashfs mount** — is well-trodden (squashfs is page-cache-backed and routinely a RO root) and is
validated on a real microVM by the e2e `e2e_cold_session_claude_harness_can_exec_ls` test on the FC
CI lane.

## Phasing (commit chain)

The harness surface (`engram-core` descriptor + `harness.proto`), the per-session `dyn_0` resolve
+ persistence, the `harness_catalog` for custom uploads, and the image clean break landed as the
A-phase commit chain (PRs #475 #476 #477 #480). Two correctness fixes surfaced when the e2e first
ran green end to end and ship with the chain: **reproducible bundle squashfs** (`SOURCE_DATE_EPOCH=0`,
PR #493 — identical content must content-address identically) and **materialize the selected
mounts at restore** (the host fetched only the snapshot's pinned bundles, never the freshly-selected
harness/skills). The **built-in-via-stamp + embedded-descriptor** model in this ADR — which retired
the earlier packed-"catalog generation" drive in favor of per-harness bundles — is the A5 follow-up
(this revision).

## Consequences

- Per-session harness selection returns, with **no cold-boot regression** (and a likely small
  win) — the thing ADR 0021 could not afford with the NBD substrate.
- The base snapshot is **simpler**: one per image, harness-agnostic, with a smaller disk.
- **The built-in harness needs no registration.** `claude` ships in the host-image stamp + the
  coordinator's embedded descriptor, so a fresh deployment runs agent sessions with **no admin
  cutover and no broken-window** after the image-harness clean break. This is the direct payoff of
  the per-harness model over the earlier "register the built-in to make it appear" design.
- **The harness path is the skills path.** Built-in = stamp (like a baked skill); custom = catalog
  + materialize (like an uploaded skill). One mechanism, one set of bugs already shaken out — no
  bespoke "catalog generation" pack/publish/pin subsystem to maintain.
- `dyn_0`'s content **varies by selected harness** (a different sha per harness), exactly as a
  skill slot varies per session — staged/materialized like any bundle, swapped in the existing
  paused window. Only the exec'd harness pages in.
- Agent sessions get **11 skill slots** (one ceded to the harness slot). Documented; size-aware
  packing remains the fallback.
- **Warm snapshots (future):** if a warm VM pool is ever introduced (none exists today), a warm
  snapshot — captured with the harness *running* — becomes per-`(image, harness)`. The cold path
  (this ADR) is unaffected; the paused-window swap stays the mechanism. Noted, no action.

## Alternatives considered

- **Keep one harness per image (status quo / ADR 0021).** Rejected: it forecloses the product
  requirement (per-session harness), forces an image re-bake to change agent driver, and its
  original justification (substrate latency + warm-snapshot coherence) no longer holds.
- **Pack the *entire* registered catalog into one shared `dyn_0` squashfs** (every harness a
  `/<name>/` subtree; selection is the argv subtree; a re-pack + re-publish + a "current generation"
  pin on every registration). The early implementation took this route. Rejected on reflection: it
  is a bespoke subsystem (a generation table, a multi-tree packer, a `bundle_pin_set` generation
  union, a host materialize of the generation) that bought only cross-session drive dedup at the
  handful-of-harnesses scale — and it forced the built-in `claude` to be *registered* before it
  worked (an admin cutover + a broken-window after the image clean break). It also produced both
  shipped correctness bugs (the non-reproducible pack and the un-materialized generation). The
  per-harness model (this ADR) makes the harness identical to a skill — built-in via the stamp,
  custom via the catalog — needs no generation machinery, and makes the built-in work with zero
  setup.
- **A dedicated 13th virtio-blk drive for the harness** (preserves 12 skill slots). Rejected for
  v1: bumps the ~13-device virtio-mmio GSI ceiling, requires a base-snapshot device-count re-bake
  + dev-vm ceiling re-probe, and forces the init-shim/`remount` paths to cover a non-`dyn_` mount
  — real correctness surface for one extra slot. Revisit only if 11 skills proves binding.
- **Bake every harness into every image, select at runtime via env.** Rejected: bloats every
  base disk with every agent runtime and re-couples harness choice to the image — the opposite of
  the goal.
