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

**Deliver the harness exactly as skills are delivered — a fleet-staged read-only squashfs
bundle, mounted on a reserved slot, swapped per session in the paused restore window — and
select it per session instead of per image.**

### 1. The harness is a bundle, on a dedicated reserved slot

The harness reuses the ADR 0055 reserved-slot pool (`AuxRoDrive::RESERVED_SLOTS = 12`,
`dyn_0..dyn_11` mounted at `/opt/engram/dyn/<i>`), with **one distinction from skills: the
harness is `exec`'d, not merely `activate`d.** A skill is content-discovered guest-side
(`engram_session_bundles::activate()` reads each slot's `mount.json` and wires what it declares),
so its slot *index* is irrelevant to the coordinator. The harness, by contrast, supplies
`argv[0]` — the coordinator must name its guest path **before** the guest mounts anything. That
demands a deterministic, coordinator-known slot.

We therefore **reserve slot 0 (`dyn_0`) for the harness**; skills assign to `dyn_1..dyn_11`.
Consequences:

- **Total aux-drive count is unchanged at 12** — no new Firecracker virtio-mmio GSI pressure
  (the ~13-device legacy-interrupt ceiling ADR 0055 §2 pins), no base-snapshot device-count
  re-bake, no dev-vm ceiling re-probe. The cost is one skill slot: **agent sessions get 11
  skills instead of 12.** A profile needing >11 skills is the same documented fallback to
  size-aware packing ADR 0055 already carries.
- `argv[0] = /opt/engram/dyn/0/harness` (the harness wrapper self-locates its sidecars from
  `$0`, so the whole pack tree lives under the mount). This revives the guest-path shape of the
  pre-0021 `harness_paths::guest_argv0` (`/run/engram/harnesses/<name>/harness`), now backed by
  squashfs instead of NBD ext4.
- The harness bundle's `mount.json` declares `{"kind":"harness","exec":"harness","args":[...]}`.
  `activate()` gains a one-line guard to **skip `kind=="harness"`** (it already skips
  `kind=="sentinel"`) so the exec'd harness is never mistaken for a skill to wire.
- `remount_bundle_mounts()` already re-parses all of `/opt/engram/dyn/` **before** the harness
  exec, which is exactly what makes exec'ing from a freshly `patch_drive`'d squashfs correct (the
  captured superblock predates the swap — the same 2026-06-03 incident-class guard skills rely
  on). **No init-shim or remount change** is needed because the harness sits on the existing
  `dyn_` path.

### 2. Capture stays harness-agnostic — already true at the process level

`cold_boot_spec` already attaches all 12 reserved slots as the **sentinel** generation
(`engram-coordinator/src/api/sessions.rs`), and base capture waits only for **agentd-ready**,
never for a harness — `build_base_snapshot` snapshots a cold VM with *no* harness process (the
harness only ever exec's at the post-restore `SpawnHarness` RPC). So slot 0 is sentinel at
capture with **no capture-path code change**: the base snapshot is harness-agnostic by
construction, one per image regardless of harness. The single upstream change is that the rootfs
no longer carries the harness binary (§4), shrinking the base disk.

### 3. Per-session resolution + mount

The harness becomes "just another selected mount," resolved by the coordinator and bound on
`dyn_0` through the unchanged host/guest swap path:

- **Wire:** `CreateSessionRequest.harness` (`optional string`) carries the per-session harness
  name. The orchestrator supplies the profile default or the per-session override (ADR 0063); it
  threads identically to `selected_skills`.
- **`resolve_harness`** takes a harness *name* (not the image manifest), resolves it against the
  **harness catalog** (§5) to `(sha256, exec_rel, args)`, builds `argv[0] =
  /opt/engram/dyn/0/<exec_rel>` with the existing backend dial flags
  (`--connect`/`--vsock-host`), and **also emits an `AuxRoDrive { drive_id: dyn_0, guest_mount:
  /opt/engram/dyn/0, fs_type: "squashfs", sha256: Some(<harness sha>) }`** merged into
  `selected_mounts`.
- **`prepare_inner`** stops reading `manifest.harness`; skill-slot assignment starts at `dyn_1`.
- An agent-mode session with **no resolvable harness is a hard create-time error**, never a
  silent sentinel boot that hangs at `SpawnHarness`. `dev_vm` mode resolves to no harness (slot 0
  stays sentinel).
- The host/guest plumbing (`restore_base_for_session` → `restore_in_jail` paused-window
  `patch_drive`) is **untouched** — the harness rides the identical path skills do.

### 4. Clean break — no compat (engram is pre-1.0, operator-curated; ADR 0021 §"No backwards
compatibility" set the precedent)

The image's `[harness]` block and the entire bake-time harness-injection path are deleted, not
dual-read:

- Drop `harness: Option<HarnessManifest>` from `ImageManifest`; delete `HarnessManifest` and its
  validators.
- Delete `engram-image-builder/src/harness.rs` (`inject_builtin_harness`, `BuiltinCatalog`,
  `Platform`, `ArtifactToml`, the canonical-path constants) and its call site; retire the
  baker-facing harness OCI tarball (`OciClient::push_harness`/`pull_harness`) in favor of the
  squashfs bundle build.
- Delete the dead `engram-coordinator/src/harness_paths.rs`; fold its one useful idea (the guest
  argv shape) into a `HARNESS_SLOT_INDEX = 0` constant.
- Remove the `ENGRAM_SESSION_HARNESS_NAME` spec-env injection (nothing consumes it) and
  `harness_name` from `image.proto` / the manifest lift; the create UI lists harnesses from the
  catalog instead.
- Strip `[harness]` from demo `engram.toml`s and re-bake images harness-free.

### 5. The harness catalog (coordinator-owned)

A small `harness_catalog` table + `MetadataStore` methods (`register_harness`,
`get_harness_by_name`, `list_harnesses`, `soft_delete_harness`), mirroring `mount_catalog` /
`CatalogSkill` and the `enabled_images` admin verbs. Each row: `name → (bundle sha256, exec_rel,
args, descriptor TOML)`. The descriptor is the **ADR 0063 `harness.toml`** content, stored so the
orchestrator/web can read it without touching the squashfs. Registration is a `RegisterHarness`
app-gRPC admin op modeled on `RegisterSkill` (built-in: record the contract pointing at the
fleet-staged `harness-claude` sha; uploaded: pack + publish a squashfs to BlobStorage via the
existing `skill_pack`).

**GC / staging:** a harness sha enters the pin universe through the same two doors as skills —
the fleet `current.json` (host-reported current generation) and `snapshots.aux_bundles` (a
session that selected harness `X` pins `X`'s sha on its eviction snapshot's `dyn_0`).
`bundle_pin_set()` is extended so live `harness_catalog` rows are also pinned, so a
registered-but-currently-unused harness stays staged and survives GC. Missing generations
materialize from `bundles/sha256/<sha>` via the existing bundle supervisor.

## Cold-boot latency — where time moves

Critical path at session **create** = base-snapshot **restore** (the ~15 s `await_agent_ready`
of ADR 0019 is *capture-time* cold boot, off the session hot path):

| Step | Today | Harness-as-bundle | Δ |
|---|---|---|---|
| `load_snapshot` (paused, UFFD) | ~5 ms | ~5 ms | 0 |
| paused-window `patch_drive` | N skills | N skills **+ 1 harness** | +1 `PATCH /drives` (µs), existing window, no new round trip |
| resume | — | — | 0 |
| `SpawnHarness` exec page-in (~0.8 s, ADR 0019) | harness faults from **rootfs over NBD** | harness faults from **local squashfs** (zstd, direct virtio-blk) | **neutral-to-faster** |
| guest harness-slot remount | (skills only) | +1 squashfs remount | µs |

Net: no GCS on the path, no new round trips, smaller base disk. The one genuinely new behavior
— **exec page-faulting from a squashfs mount** — is well-trodden (squashfs is page-cache-backed
and routinely a RO root), but is validated on a real microVM by a harness-from-squashfs loopback
test (mirroring `aux_ro_drive.rs`), wired into the FC CI lane.

## Phasing (living checklist; each phase = one worktree + one PR, ADR-bookended)

- [ ] **A1** — `engram-core` harness descriptor + `harness.proto` messages + converter (shared
  with ADR 0063 A1). *(branch off main)*
- [ ] **A2** — harness-claude squashfs bundle build + fleet staging (inert; host reports it in
  `current_bundles`). *(off A1)*
- [ ] **A3** — coordinator `harness_catalog` + `RegisterHarness` + `HARNESS_SLOT_INDEX=0` +
  `resolve_harness` rewrite + skills→`dyn_1..` + `activate()` skip + `CreateSessionRequest.harness`
  + harness-from-squashfs FC loopback test in `ci.yml`. After this, sessions exec the harness
  **from the bundle**; the still-baked rootfs harness is dead weight. *(off A1)*
- [ ] **A4** — clean-break removals (§4) + re-bake demo images harness-free; confirm capture
  stays harness-agnostic (smaller rootfs). *(off A3)*

## Consequences

- Per-session harness selection returns, with **no cold-boot regression** (and a likely small
  win) — the thing ADR 0021 could not afford with the NBD substrate.
- The base snapshot is **simpler**: one per image, harness-agnostic, with a smaller disk.
- Agent sessions get **11 skill slots** (one ceded to the harness). Documented; size-aware
  packing remains the fallback.
- **Warm snapshots (future):** if a warm VM pool is ever introduced (none exists today), a warm
  snapshot — captured with the harness *running* — becomes per-`(image, harness)`. The cold path
  (this ADR) is unaffected; the paused-window swap stays the mechanism. Noted, no action.

## Alternatives considered

- **Keep one harness per image (status quo / ADR 0021).** Rejected: it forecloses the product
  requirement (per-session harness), forces an image re-bake to change agent driver, and its
  original justification (substrate latency + warm-snapshot coherence) no longer holds.
- **A dedicated 13th virtio-blk drive for the harness** (preserves 12 skill slots). Rejected for
  v1: bumps the ~13-device virtio-mmio GSI ceiling, requires a base-snapshot device-count re-bake
  + dev-vm ceiling re-probe, and forces the init-shim/`remount` paths to cover a non-`dyn_` mount
  — real correctness surface for one extra slot. Revisit only if 11 skills proves binding.
- **Bake every harness into every image, select at runtime via env.** Rejected: bloats every
  base disk with every agent runtime and re-couples harness choice to the image — the opposite of
  the goal.
