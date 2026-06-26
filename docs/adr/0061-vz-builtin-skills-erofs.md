# ADR 0061: built-in skills on the VZ backend (erofs RO drives + dev-stack staging)

**Status:** Accepted

**Related:** ADR 0055 (dynamic per-session directory mounts — the skill model
this extends to VZ), ADR 0035 (host-side bundle generation store + the
`current.json` stamp / `current_bundles` heartbeat), ADR 0027 (RO session
bundles), ADR 0003 (the VZ backend), ADR 0020 (base-snapshot session restore),
ADR 0024 (unified dev orchestration / the Tiltfile + `detect-backend.sh`).
Builds on the `6747b6cd` refactor that dropped virtio-fs to unify both backends
on read-only virtio-blk images.

## Context

Enabling the built-in **Built-in skills** option (the bundle named `skills`) and
creating a session on a local macOS dev stack returns a 400:

> skill `skills` is unknown (not a staged fleet bundle and not in the upload
> catalog), or no host has reported its bundle yet

This is two separate gaps stacked on top of each other.

### Gap 1 — the dev host reports no bundles (the 400)

`resolve_selected_skills` (`crates/engram-coordinator/src/api/sessions.rs:952`)
resolves each selected skill name against **the fleet stamp ∪ the org upload
catalog**. The fleet stamp is `fleet_bundle_catalog()` — any active host's
`current_bundles` (`name → sha256`), which the host populates from a
`current.json` stamp read **once at startup** via
`bundles::read_stamp(bundle_dir_from_env())`
(`crates/engram-host-agent/src/lib.rs:1018`). `skills` is a baked built-in, never
written to the upload catalog, so it must resolve via the stamp.

But the dev stack never satisfies the precondition ADR 0055 spelled out
("dev/CI must stage bundles before the host-agent starts", ADR 0055 line
316-318):

- The Tiltfile's `host_agent_resource` never sets `ENGRAM_BUNDLE_DIR`
  (`Tiltfile:394`), so `bundle_dir_from_env()` falls back to the Linux
  fleet path `/var/lib/engram/shared`, absent on macOS.
- `just dev` (just `tilt up`) never stages a `current.json` stamp;
  `var/shared/` and `var/bundles/` don't exist until a developer manually runs
  `just bundles` / `just bundles-squashfs`.

So `read_stamp` finds nothing, the host heartbeats empty `current_bundles`, and
`assign_skill_slots` (`sessions.rs:1037`) 400s. This affects **all** local dev
modes, not just VZ.

### Gap 2 — VZ never mounts skill bundles, and the Kata kernel can't read squashfs

Even past the 400, skills don't reach a VZ guest:

- VZ is wrapped in `PooledBackend` (`crates/engram-host-agent/src/lib.rs:269` —
  the "FC-only" comment in `vz/src/snapshot.rs` refers to BlobStorage *upload*,
  not the wrapper). `PooledBackend.restore_base_for_session` →
  `restore_with(fresh=true, selected_mounts)` → `inner.restore_fresh(metadata,
  selected_mounts)`. The VZ backend does **not** override `restore_fresh`, so the
  trait default (`crates/engram-core/src/traits/sandbox.rs:415`) silently drops
  `selected_mounts` and delegates to `restore`. The VZ backend has zero
  aux-RO-drive code (`vz/src/snapshot.rs:107,152` — `aux_bundles: vec![]`,
  `aux_ro_drives: Vec::new()`).
- The FC path attaches each bundle as a content-addressed **squashfs** on a RO
  virtio-blk drive; the guest init shim (`engram-image-builder` default
  `engram-init`, lib.rs:311-321) iterates `/dev/vd*`, skips the rootfs, and
  `mount -t squashfs -o ro`s every squashfs device at `/opt/engram/dyn/<i>`.
  **The VZ guest kernel (the Kata static arm64 kernel from `just pull-kernel`)
  has no `CONFIG_SQUASHFS`** — confirmed by inspecting the cached
  `vmlinux-arm64`: zero squashfs driver strings, but a full **erofs** driver
  (`erofs_read_superblock`, …) and `ext4`/`virtiofs`/`overlay`/`erofs` in the
  filesystem registration list. So the FC squashfs path cannot be reused
  verbatim — `mount -t squashfs` would fail on VZ.

## Decision

Make built-in skills work end-to-end on the VZ backend, with **fresh-create and
resume** parity, by (a) staging bundles + a stamp in the dev stack before the
host-agent starts, and (b) attaching each bundle as a content-addressed
**erofs** read-only virtio-blk image — the same "plug in a read-only disc" shape
the FC path uses with squashfs, in the one RO filesystem format the Kata kernel
can mount. No coordinator, orchestrator, or web changes: `resolve_selected_skills`
and the stamp are VMM-agnostic; only the on-disk image format and the one-line
guest mount differ between backends.

### Part 1 — dev-stack staging (fixes the 400, both backends)

- **`flake.nix`**: add `erofs-utils` to the dev shell (gives `mkfs.erofs` under
  `nix develop`; `squashfsTools` + `e2fsprogs` are already there). Outside nix,
  `brew install erofs-utils`.
- **`justfile`**: new `bundles-vz` recipe mirroring `bundles-squashfs` — builds
  an **erofs** image per bundle (`skills` always; `playwright` /
  `integrations-cli` best-effort, skipped without Docker, exactly as `just
  bundles` already degrades), content-addressed as `var/shared/<sha>.erofs`, and
  writes `current.json` (`name → sha`, extension-free). The stamp is fs-agnostic;
  FC appends `.squashfs`, VZ appends `.erofs`.
- **`Tiltfile`**: a `bundles` `local_resource` that runs the recipe matching the
  detected backend (vz → `bundles-vz`, fc → `bundles-squashfs`). `host-agent`
  gains `ENGRAM_BUNDLE_DIR=$PWD/var/shared` in its `serve_env` and
  `resource_deps=['bundles']`. Wire the dependency so **rebuilding `bundles`
  re-triggers a `host-agent` restart** — the host re-reads the stamp, so an
  edited skill propagates with `rebuild → new session` (no manual restart). This
  matches the harness-substrate model (`6747b6cd`: "changes don't propagate to
  live sandboxes — the coord rebuilds on restart").

With a non-empty `current.json` under `ENGRAM_BUNDLE_DIR` before the host-agent
boots, `read_stamp` populates `current_bundles`, and `resolve_selected_skills`
resolves `skills` → its sha → a reserved slot.

### Part 2 — the VZ backend attaches the drives

- **`VzConfig.bundle_dir: PathBuf`**, set from `bundles::bundle_dir_from_env()`
  where the host-agent constructs the VZ backend (`host-agent/src/main.rs:411`).
- **`VzVmConfig` + `build_vm`** (`vz/src/vm.rs`): thread `aux_ro_drives` +
  `bundle_dir`. After attaching the rootfs (`/dev/vda`), attach each drive **with
  `sha256 = Some`** as a RO `VZDiskImageStorageDeviceConfiguration` from
  `bundle_dir/<sha>.erofs`, in slot order (`/dev/vdb`, `/dev/vdc`, …). Sentinel
  slots (`sha = None`) are skipped — so base-snapshot capture and cold-create,
  whose specs carry only `(0..RESERVED_SLOTS).map(reserved_slot)` placeholders,
  attach nothing, which is correct (the base snapshot stays skill-agnostic).
- **`restore_fresh(metadata, selected_mounts)` override** (the key hook): fold
  `selected_mounts` into `spec.aux_ro_drives`, store the resolved spec on the live
  sandbox state, build the VM → **skills mount on fresh create**.
- **`restore(metadata)`**: attach `sha = Some` drives from
  `manifest.spec.aux_ro_drives` → **skills survive resume**.
- **`snapshot()`**: already clones `live.spec`; because `restore_fresh` / `create`
  store the resolved drives there, the manifest records them and resume
  re-attaches the same `<sha>.erofs`. (`aux_bundles` stays `[]`: VZ snapshots are
  host-local, no BlobStorage upload — unchanged from today.)

### Part 3 — the guest init shim (one safe line)

- **`engram-image-builder`** default init shim dyn-mount loop: try squashfs, then
  erofs — `mount -t squashfs -o ro "$dev" … || mount -t erofs -o ro "$dev" …`.
  Both are read-only filesystems, so the loop still **never** matches FC's
  read-write **ext4** harness/CA-stage drive (the existing exclusion holds by
  construction). FC squashfs is tried first and is unaffected; VZ erofs is the
  fallback. Requires a guest-image **re-bake** (`just bake-demo`) + a fresh
  session to take effect (the shim is baked into the rootfs).
- **`engram-agentd/src/remount.rs` needs no change**: it filters to `fs_type ==
  "squashfs"` (`remount.rs:69`), so it correctly skips VZ's erofs mounts. There
  is no `patch_drive`/device-swap on VZ, so no re-parse is needed — the
  init-shim mount is already correct, and `activate()` reads each mount's
  `mount.json` regardless of fs type.

## Alternatives considered

- **squashfs on VZ (exact FC path).** Rejected: the Kata VZ kernel has no
  `CONFIG_SQUASHFS` (verified). Would require building and shipping a custom VZ
  guest kernel — far more than the feature warrants.
- **ext4 RO images on VZ.** Tooling is already in the flake (`mke2fs -d`,
  confirmed working). Rejected: ext4 is a read-write filesystem, and the shared
  guest init shim **deliberately excludes ext4** so it never mounts FC's ext4
  harness/CA-stage drive as a dyn slot. Reusing ext4 for skills would force the
  shared shim to distinguish skill-bundle from harness-substrate by something
  other than fs type — per-VMM branching or fragile device tracking. erofs, being
  read-only, disambiguates by construction (mount squashfs-or-erofs, never ext4).
- **virtiofs directory share.** The Kata kernel supports it and the
  `objc2-virtualization 0.3.2` bindings exist
  (`VZVirtioFileSystemDeviceConfiguration`, `VZSharedDirectory`). Rejected on
  principle and on parity: virtio-fs is exactly what `6747b6cd` **removed** to
  unify both backends on RO virtio-blk, because mainline Firecracker doesn't
  support it. It also doesn't fit the content-addressed bundle interface — a
  directory share has no content identity, so it can't be pinned into a snapshot
  (`aux_bundles` / the GC pin set), which **breaks the resume-determinism this
  ADR is required to preserve**; and it would force a VZ-only field on the shared
  `SandboxSpec` plus a VZ-only `mount -t virtiofs` branch in the shared init
  shim. Its only advantage — live skill edits with no rebuild — is outweighed,
  and the rebuild cost is mitigated by the Tilt auto-restart (Part 1).

## Consequences

- **Faithful to prod, including the propagation model.** Skills stay decoupled
  from the image/base snapshot and bind per session against the host's current
  generation; an edit ships on the next "host roll" (a host-agent restart),
  identical to FC and to the harness substrate. New sessions pick up a rebuilt
  bundle after the host-agent restarts; **live and resumed sessions keep their
  pinned generation** (resumed sessions re-attach the exact `<sha>.erofs` in
  their snapshot) — by design.
- **Re-bake required once** for the init-shim change to land in the dev guest
  image. Called out in the implementation plan and the dev runbook.
- **No coordinator/orchestrator/web changes**; the e2e/FC lanes still exercise
  the squashfs path (init shim tries squashfs first), so FC parity is preserved.

## Testing

- VZ backend integration test (macOS, `just vz-test`, requires the
  virtualization entitlement): boot a guest with one erofs skill drive attached;
  assert `/opt/engram/dyn/0` is mounted and the bundle's `mount.json` / wrappers
  are present.
- `engram-image-builder` unit test: assert the dyn-mount line accepts both
  squashfs and erofs (and still excludes ext4).
- Resume parity: snapshot a VZ session with a skill attached, restore, assert the
  same `<sha>.erofs` re-attaches and the skill is present.
- FC is not regressed: the init-shim change is squashfs-first; the e2e stack and
  FC integration lanes continue to exercise squashfs.

## As-built (filled in between phases / at Accepted)

### Commit chain (branch `adr-0061-vz-builtin-skills`)

| SHA | Subject |
|---|---|
| `0f834f6a` | `docs(adr): ADR 0061 (Proposed)` |
| `f0e8048e` | `build(flake): add erofs-utils` |
| `89f4de75` | `feat(just): bundles-vz — content-addressed erofs bundles` |
| `be68677e` | `feat(tilt): stage bundles + ENGRAM_BUNDLE_DIR; restart host-agent on bundle change` |
| `71351000` | `feat(vz): VzConfig.bundle_dir + staged_erofs_path; thread from host-agent` |
| `4cfc0613` | `feat(vz): attach skill bundles as read-only erofs virtio-blk drives` |
| `a37f4592` | `feat(vz): bind skills on restore_fresh, re-attach on resume` |
| `af5ddf6e` | `test(vz): erofs skill drive attaches + mounts (live #[ignore] test)` |
| `e5eab619` | `feat(init): squashfs \|\| erofs fallback in dyn-mount loop` |
| `7c8f93ff` | `docs(runbook): VZ skill bundle edit + re-bake loop` |
| `240d16e4` | `fix(vz): restore clone-stays-intact rationale comment in restore_impl` |
| `76d41112` | `docs(runbook): re-bake uses a fresh session, not a stack restart` |
| `d09fa9fe` | `fix(just): pack VZ skill erofs with -b 4096 to match guest page size` |

### Decisions and divergences

**erofs path is VZ-local; the shared `AuxRoDrive` stays `.squashfs`.** VZ derives the
staged path as `<bundle_dir>/<sha>.erofs` via `vm::staged_erofs_path`; the shared
`AuxRoDrive::staged_file_name` continues to return `.squashfs` for FC. The wire
`fs_type` from the coordinator (`"squashfs"`) is cosmetic on VZ — the backend resolves
its own `.erofs` path and the init shim mounts squashfs-or-erofs transparently.

**`snapshot()` needed no change.** It already clones `live.spec`, which
`restore_impl`/`create` populate with the resolved drives. Resume therefore
re-attaches the same `<sha>.erofs` from the manifest with no extra logic.

**`engram-agentd/src/remount.rs` needed no change.** Its `fs_type == "squashfs"`
filter (`remount.rs:69`) correctly skips VZ's erofs mounts. VZ does no `patch_drive`
device swap — the init-shim mount is already correct — so `activate()` reads each
mount's `mount.json` regardless of fs type.

**`restore`/`restore_fresh` collapse to one-line delegators over `restore_impl`.** The
shared implementation takes `(metadata, Option<Vec<AuxRoDrive>>)`: `restore_fresh`
passes `Some(selected_mounts)` (replacing drives), `restore` passes `None` (keeping
the pinned drives from the manifest). This eliminated a previously drafted two-path
approach and kept the code minimal.

**Tilt auto-restart is scoped to `deps=[current.json]` only.** The host-agent gains
`trigger_mode=TRIGGER_MODE_AUTO` but only fires when `current.json` changes — not on
every `.rs` source edit — so the inner-loop build latency is unaffected. Source
changes still require a manual Tilt trigger.

**Init-shim exclusion clarification (review catch).** The init-shim unit test's
ext4-exclusion assertion is scoped to the dyn-mount loop section only. The
CA-staging block above the loop legitimately uses `mount -t ext4` (the harness
substrate); the dyn-mount loop itself never emits `ext4`, so the exclusion holds.

**`PooledBackend` macOS dispatch is correct without changes.** `PooledBackend`'s macOS
path calls `inner.restore_fresh(metadata, selected_mounts)` — the override hook — so
the new VZ impl is reached. Bundle materialize/publish are gated on non-empty
`aux_bundles`; VZ leaves `aux_bundles: []`, so VZ never hits the `.squashfs`
BlobStorage upload path.

**erofs block size must be forced to 4 KiB (live-validation catch, `d09fa9fe`).**
`mkfs.erofs` defaults its block size to the *host* page size. On macOS/Apple Silicon
that is 16 KiB (blkszbits 14), but the Linux guest uses 4 KiB pages and erofs requires
block size ≤ page size. The first live test attached `/dev/vdb` and the init shim tried
the correct `mount -t erofs`, but the mount failed (`dmesg: erofs: blkszbits 14 isn't
supported`) and the skill never appeared. The `bundles-vz` recipe now passes `-b 4096`.
This is the inverse of the risk noted in the plan's self-review (which assumed the
default was already 4 KiB).

### Live validation (confirmed on macOS/VZ, 2026-06-26)

Validated on a running `just dev` VZ stack against the re-baked `demo-claude:warm-1`
(new init shim) with a 4 KiB-block skills bundle:

- **No 400** — `CreateSession` with `selected_skills:["skills"]` returns a session
  (previously: `skill \`skills\` is unknown …`).
- **erofs drive attached** — `/dev/vdb` present in the guest (restore_fresh).
- **Auto-mounted** — `/proc/self/mounts`:
  `/dev/vdb /opt/engram/dyn/0 erofs ro,relatime,…`; `/opt/engram/dyn/0` contains the
  skill's `bin/`, `skills/`, and `mount.json`
  (`{"kind":"skill","skills":[{"name":"share-file",…}]}`).

Still deferred (not force-tested): resume re-attach parity — idle-eviction was paused by
disk pressure on the test box, so the session could not be driven to a snapshot/restore.
The `restore` (resume) path is the same `restore_impl` shared with the validated
fresh-create path, reading `manifest.spec.aux_ro_drives`.
