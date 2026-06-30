# ADR 0062: Per-session harness selection via the RO-bundle engine

Status: 2026-06-29 — **Proposed.**
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

**Pack the *entire registered harness catalog* — built-in and custom, every harness an OCI
artifact — into one content-addressed read-only squashfs, mount it fleet-wide on a single
reserved slot (`dyn_0`), and select a harness per session purely by which subtree `argv[0]`
points at.** The drive content is identical across all sessions at a given catalog version, so
it is staged once per host and shared — the dedup is across sessions, not per-harness bundles.

### 1. One catalog drive on `dyn_0`, selection by argv subtree

The harness rides the ADR 0055 reserved-slot pool (`AuxRoDrive::RESERVED_SLOTS = 12`,
`dyn_0..dyn_11` mounted at `/opt/engram/dyn/<i>`), but differs from skills in two ways:

1. **It is `exec`'d, not merely `activate`d.** A skill is content-discovered guest-side
   (`engram_session_bundles::activate()` reads each slot's `mount.json` and wires it), so its slot
   *index* is irrelevant to the coordinator. The harness supplies `argv[0]` — the coordinator must
   name its guest path *before* the guest mounts anything — so it needs a deterministic,
   coordinator-known slot. We **reserve slot 0 (`dyn_0`)**; skills assign to `dyn_1..dyn_11`.
2. **The drive carries the whole catalog, not one selected harness.** `dyn_0`'s squashfs holds
   *every* registered harness as a sibling subtree: `/<name>/` per harness, entry at
   `/<name>/<exec>`. A session picks its harness by `argv[0] = /opt/engram/dyn/0/<name>/<exec>`;
   the harness wrapper self-locates its sidecars (runtime, CLI) from `$0`, so each subtree is
   self-contained and harnesses never collide. This revives the pre-0021
   `harness_paths::guest_argv0` shape (`/run/engram/harnesses/<name>/harness`), now backed by one
   shared catalog squashfs.

Why the whole catalog on one drive rather than per-session-swapping the selected harness:

- **Dedup across sessions.** Every session mounts the *same* `dyn_0` content (the current catalog
  generation), regardless of which harness it runs. One staged file per host, shared by all — no
  per-harness staging churn and no per-session variance in the drive.
- **The per-session restore swap becomes uniform.** Base snapshots capture `dyn_0` as the
  sentinel (catalog-agnostic — see §2), and every session's paused-window `patch_drive` targets
  the *same* current catalog generation. Selection moved entirely into `argv` (host-disk-free,
  zero-latency), out of the drive content.
- **Total aux-drive count is unchanged at 12** — no new virtio-mmio GSI pressure (the ~13-device
  legacy-interrupt ceiling ADR 0055 §2 pins), no base-snapshot device-count re-bake, no dev-vm
  ceiling re-probe. The cost is one skill slot: **agent sessions get 11 skills instead of 12**
  (the documented fallback to size-aware packing if a profile needs more).

The catalog squashfs carries a top-level `mount.json` `{"kind":"harness"}`; `activate()` gains a
one-line guard to **skip `kind=="harness"`** (it already skips `kind=="sentinel"`) so the
exec'd catalog is never mistaken for a skill to wire. `remount_bundle_mounts()` already re-parses
all of `/opt/engram/dyn/` **before** the harness exec, which is what makes exec'ing from a freshly
`patch_drive`'d squashfs correct (the captured superblock predates the swap — the same
2026-06-03 incident-class guard skills rely on). **No init-shim or remount change** is needed —
the catalog sits on the existing `dyn_` path.

### 2. Capture stays catalog-agnostic — already true at the process level

`cold_boot_spec` already attaches all 12 reserved slots as the **sentinel** generation
(`engram-coordinator/src/api/sessions.rs`), and base capture waits only for **agentd-ready**,
never for a harness — `build_base_snapshot` snapshots a cold VM with *no* harness process (the
harness only ever exec's at the post-restore `SpawnHarness` RPC). So `dyn_0` is sentinel at
capture with **no capture-path code change**: the base snapshot is harness-agnostic by
construction, one per image, and — crucially — **catalog-version-agnostic**, so registering a
harness never forces a base re-capture. The single upstream change is that the rootfs no longer
carries any harness binary (§4), shrinking the base disk.

### 3. Per-session resolution + mount

The catalog is "just another selected mount" bound on `dyn_0` through the unchanged host/guest
swap path; the harness *name* only chooses the argv subtree:

- **Wire:** `CreateSessionRequest.harness` (`optional string`) carries the per-session harness
  name. The orchestrator supplies the profile default or the per-session override (ADR 0063); it
  threads identically to `selected_skills`.
- **`resolve_harness`** takes a harness *name* (not the image manifest), validates it against the
  **harness catalog** (§5), looks up that harness's launch contract (`exec`, `args` from its
  `harness.toml`), and emits:
  - `argv[0] = /opt/engram/dyn/0/<name>/<exec>` with the existing backend dial flags
    (`--connect`/`--vsock-host`) + the harness's `args`;
  - an `AuxRoDrive { drive_id: dyn_0, guest_mount: /opt/engram/dyn/0, fs_type: "squashfs",
    sha256: Some(<current catalog generation sha>) }` merged into `selected_mounts` — **the same
    sha for every session at a given catalog version** (the harness name does not affect it).
- **`prepare_inner`** stops reading `manifest.harness`; skill-slot assignment starts at `dyn_1`.
- An agent-mode session whose harness name **is not in the catalog is a hard create-time error**,
  never a silent sentinel boot that hangs at `SpawnHarness`. `dev_vm` mode resolves to no harness
  (`dyn_0` stays sentinel — the catalog isn't mounted for a pure dev VM).
- The host/guest plumbing (`restore_base_for_session` → `restore_in_jail` paused-window
  `patch_drive`) is **untouched** — the catalog rides the identical path skills do.

### 4. Clean break — every harness is an OCI artifact in the catalog; none touch images

There is no longer a "built-in vs custom" distinction at the image layer: **all harnesses are
OCI artifacts registered into the catalog** (§5). A built-in like `claude` is just a published
GHCR artifact pre-registered at bootstrap; a custom harness is the operator's own OCI artifact
registered the same way. The image's `[harness]` block and the entire bake-time harness-injection
path are deleted, not dual-read (engram is pre-1.0, operator-curated; ADR 0021 §"No backwards
compatibility" set the precedent):

- Drop `harness: Option<HarnessManifest>` from `ImageManifest`; delete `HarnessManifest` + its
  validators.
- Delete `engram-image-builder/src/harness.rs` (`inject_builtin_harness`, `BuiltinCatalog`,
  `Platform`, `ArtifactToml`, the canonical-path constants) and its call site. The harness OCI
  *artifact* format stays (it is now the registration input, pulled by the coordinator, not the
  baker); the **bake-time injection** into the rootfs is what's removed.
- Delete the dead `engram-coordinator/src/harness_paths.rs`; fold its one useful idea (the guest
  argv shape) into a `HARNESS_SLOT_INDEX = 0` constant + the catalog layout.
- Remove the `ENGRAM_SESSION_HARNESS_NAME` spec-env injection (nothing consumes it) and
  `harness_name` from `image.proto` / the manifest lift; the create UI lists harnesses from the
  catalog instead.
- Strip `[harness]` from demo `engram.toml`s and re-bake images harness-free.

### 5. The harness catalog (coordinator-owned) — OCI in, one packed squashfs out

A `harness_catalog` table + `MetadataStore` methods (`register_harness`, `get_harness_by_name`,
`list_harnesses`, `soft_delete_harness`), mirroring `mount_catalog` / `CatalogSkill` and the
`enabled_images` admin verbs. Each row: `name → (oci_ref + resolved digest, launch contract
exec/args, descriptor TOML, extracted-tree content hash)`. The descriptor is the **ADR 0063
`harness.toml`** content (it carries both the launch contract `exec`/`args` *and* the
orchestrator-facing env contract), stored so the orchestrator/web can read it without touching
the squashfs.

**Registration** is a `RegisterHarness(name, oci_ref)` app-gRPC admin op (modeled on
`RegisterSkill`), uniform for built-in and custom:
1. Pull the OCI artifact (reuse `engram_oci`), extract its tree, read + validate its
   `harness.toml`.
2. Upsert the catalog row.
3. **Re-pack the catalog squashfs** from *all* live rows' trees — each under `/<name>/` — via the
   existing `skill_pack` machinery, content-addressed, and publish it to BlobStorage. Record the
   resulting sha as the **current catalog generation**.

Built-in `claude` is registered at bootstrap from its published GHCR artifact (a seed op), so a
fresh deployment has a usable catalog with no manual step.

**GC / staging:** the catalog generation sha enters the pin universe through the same doors as
skills — `snapshots.aux_bundles` (every live/evicted session pins the catalog generation its
`dyn_0` references) **plus** a pin on the *current* catalog generation (so a fresh session can
always mount it). `bundle_pin_set()` is extended to include the current catalog generation.
Hosts materialize a missing generation from `bundles/sha256/<sha>` via the existing bundle
supervisor (exactly the ADR 0055 P2 uploaded-skill path); the current generation may also be
pre-staged into the FC-host image via node-assets for cold-host readiness. An old generation is
reclaimed once its session pins drain.

## Cold-boot latency — where time moves

Critical path at session **create** = base-snapshot **restore** (the ~15 s `await_agent_ready`
of ADR 0019 is *capture-time* cold boot, off the session hot path):

| Step | Today | Catalog-on-`dyn_0` | Δ |
|---|---|---|---|
| `load_snapshot` (paused, UFFD) | ~5 ms | ~5 ms | 0 |
| paused-window `patch_drive` | N skills | N skills **+ 1 catalog** | +1 `PATCH /drives` (µs), existing window, no new round trip |
| resume | — | — | 0 |
| `SpawnHarness` exec page-in (~0.8 s, ADR 0019) | harness faults from **rootfs over NBD** | the **selected** harness's subtree faults from the local catalog squashfs (zstd, direct virtio-blk) | **neutral-to-faster** |
| guest catalog-slot remount | (skills only) | +1 squashfs remount | µs |

Mounting the catalog squashfs is metadata-only (the superblock + root inode); a session pays
page-in **only for the harness it actually exec's** — the other registered harnesses sit cold in
the same squashfs and cost host disk, not boot latency. Net: no GCS on the path, no new round
trips, smaller base disk. The one genuinely new behavior — **exec page-faulting from a squashfs
mount** — is well-trodden (squashfs is page-cache-backed and routinely a RO root), but is
validated on a real microVM by a harness-from-squashfs loopback test (mirroring `aux_ro_drive.rs`),
wired into the FC CI lane.

## Phasing (living checklist; each phase = one worktree + one PR, ADR-bookended)

- [x] **A1** — `engram-core` harness descriptor + `harness.proto` `ListHarnesses` read surface
  (shared with ADR 0063 A1). The launch-contract fields (`exec`/`args`) and the engram-core→proto
  converter land with their consumers in A2/A3, not as dead code here. *(branch off main — PR
  #475)*
- [ ] **A2** — catalog-packing mechanism: descriptor launch contract (`exec`/`args`),
  `engram-mount-manifest` `KIND_HARNESS`, and the catalog packer (given N extracted harness trees
  → one content-addressed `/<name>/` squashfs via `skill_pack`). Unit-testable in isolation.
  *(off A1)*
- [ ] **A3** — coordinator `harness_catalog` + `RegisterHarness(name, oci_ref)` (OCI pull →
  extract → validate → upsert → re-pack catalog → publish blob → bump generation) + built-in
  `claude` bootstrap seed + `HARNESS_SLOT_INDEX=0` + `resolve_harness` rewrite (catalog-generation
  mount on `dyn_0` + argv subtree) + skills→`dyn_1..` + `activate()` skip +
  `CreateSessionRequest.harness` + `bundle_pin_set` extension + harness-from-squashfs FC loopback
  test in `ci.yml`. After this, sessions exec the harness **from the catalog**; the still-baked
  rootfs harness is dead weight. *(off A1)*
- [ ] **A4** — clean-break removals (§4) + re-bake demo images harness-free; confirm capture
  stays harness/catalog-agnostic (smaller rootfs). *(off A3)*

## Consequences

- Per-session harness selection returns, with **no cold-boot regression** (and a likely small
  win) — the thing ADR 0021 could not afford with the NBD substrate.
- The base snapshot is **simpler**: one per image, harness- and catalog-agnostic, with a smaller
  disk. Registering a harness never re-captures a base snapshot.
- **Built-in and custom harnesses are unified** — both are OCI artifacts registered into the
  catalog. No bake-time injection, no per-image harness coupling, no `[harness]` block.
- **Host disk holds the whole catalog.** `dyn_0`'s squashfs carries every registered harness
  (each ~100 MB+ with its bundled runtime), staged once per host (deduped across sessions). A
  bounded, modest cost; it does not affect boot latency (only the exec'd harness pages in).
- **A catalog mutation re-packs + re-stages the full catalog blob** (squashfs has no incremental
  append). Registration is a rare admin op off the hot path, so this is acceptable at the
  expected handful-of-harnesses scale; if the catalog ever grows large, per-harness blobs with a
  guest-side overlay/union mount is the documented future optimization.
- Agent sessions get **11 skill slots** (one ceded to the catalog drive). Documented; size-aware
  packing remains the fallback.
- **Warm snapshots (future):** if a warm VM pool is ever introduced (none exists today), a warm
  snapshot — captured with the harness *running* — becomes per-`(image, harness)`. The cold path
  (this ADR) is unaffected; the paused-window swap stays the mechanism. Noted, no action.

## Alternatives considered

- **Keep one harness per image (status quo / ADR 0021).** Rejected: it forecloses the product
  requirement (per-session harness), forces an image re-bake to change agent driver, and its
  original justification (substrate latency + warm-snapshot coherence) no longer holds.
- **Per-session-swap the *selected* harness bundle onto `dyn_0`** (one content-addressed squashfs
  per harness, swapped to the chosen one each session). Workable, but the drive content then
  varies per session (per-harness staging churn) and the swap target differs across sessions for
  no benefit. Packing the whole catalog makes the drive *identical* across sessions — staged once,
  shared — and moves selection into `argv` (host-disk-free). Rejected in favor of the catalog
  drive.
- **A dedicated 13th virtio-blk drive for the harness** (preserves 12 skill slots). Rejected for
  v1: bumps the ~13-device virtio-mmio GSI ceiling, requires a base-snapshot device-count re-bake
  + dev-vm ceiling re-probe, and forces the init-shim/`remount` paths to cover a non-`dyn_` mount
  — real correctness surface for one extra slot. Revisit only if 11 skills proves binding.
- **Bake every harness into every image, select at runtime via env.** Rejected: bloats every
  base disk with every agent runtime and re-couples harness choice to the image — the opposite of
  the goal.
