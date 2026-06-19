# ADR 0055: Dynamic per-session directory mounts — profile-selected skills on the RO-bundle engine

Status: 2026-06-19 — **Proposed.** Builds on **ADR 0027** (the read-only host-mounted
shared bundle engine), **ADR 0035** (content-addressed bundle generations + the load-paused
`patch_drive` swap), and **ADR 0053** (session profiles). This is the first concrete
instance of the **per-profile capability scoping ADR 0053 §7 explicitly deferred to "its
own ADR"**, and it realizes the **"dynamic per-session skill/tool selection" ADR 0027
deferred**.

Revised 2026-06-19 (P1 underway): the reserved-slot count drops from a speculative **N=64
stress test** to a small **~12**, after source research on the vendored FC fork showed x86
virtio-mmio (engrams boots `pci=off`) caps total devices at the 19-line legacy GSI pool —
so ~13 aux drives, and N=64 was never possible on the production transport. The uniform
one-skill-per-drive model is unchanged; size-aware packing remains the documented fallback
if a profile ever needs more skills than fit. Threaded through §2, the measurement gate,
Consequences, Alternatives (a new `pci=on` rejection), and Implications.

## Context

Today the *set* of read-only mounts a session gets is **frozen at image-enable**
(ADR 0027). Two bundles, both static and image-scoped:

- **`skills.squashfs`** — always attached at `/opt/engram/skills` (the `share-file` /
  `create-pull-request` wrappers + `SKILL.md` dirs);
- **`playwright.squashfs`** — opt-in via a `[browser] enabled` flag in the image
  manifest, attached at `/opt/engram/browser` (`show-your-work` + chromium).

Both are declared per-image, baked into the FC-host image at a content-addressed path,
and replayed identically for **every** session of that image. There is no per-session
selection: a session inherits exactly the device model its base snapshot was captured
with, because **Firecracker can only attach drives before boot, never after
`load_snapshot`** (its device model is virtio-blk/net/vsock/rng/balloon; no virtiofs,
no hot-plug).

We want **dynamic, per-session directory mounts**. The first use case: **dynamic skills,
selected per session as part of an orchestrator profile** — an admin (and later a user)
curates a profile that says "sessions started here mount *these* skills." This is exactly
the capability scoping ADR 0053 shaped profiles to grow into.

### The hard constraint: fast boot

Boot latency is non-negotiable (ADR 0019/0020). The facts that govern this design:

- **There is no warm VM pool.** Every session **create** is a full **per-image
  base-snapshot restore** (ADR 0020 cold-boot-via-restore). There is no pool of
  pre-booted/pre-resumed VMs handing out ready guests — the only thing in the system
  called a "warm pool" is the NBD `/dev/nbdN` *device-slot* allocator (ADR 0049), which
  pools kernel device handles, not VMs.
- **Base snapshots are keyed per-image, not per-session.** One base snapshot per enabled
  image; capture happens once, at image-enable, not per session.
- **Everything session-specific is late-bound after restore, in a per-session *paused*
  window.** `restore_base_for_session` does `load_snapshot` **paused** (`resume_vm =
  false` whenever there is an aux-drive swap to apply), applies per-session state, then
  `resume`s (`engram-sandbox-firecracker/src/lib.rs`, the load + `fc.swap_aux_bundles`
  block). ADR 0035 already uses this exact window to `patch_drive` aux drives whose
  content generation drifted from the host's current one.

The failure mode that would wreck boot latency is needing a distinct base snapshot per
skill-combination. So the design rule is:

> **The base snapshot stays skill-agnostic. Selected skills bind in the per-session
> paused window after restore — never baked into captured state.**

ADR 0035's content-addressed `patch_drive` swap is most of the machinery: it already
swaps *which bytes* a drive points at, in the paused window, per session. We extend it
from "swap a drive to its current generation" to "swap a reserved slot to a selected
skill."

## Decision

**One uniform model: a skill is a content-addressed read-only directory.** Every catalog
entry — `share-file`, `create-pull-request`, `show-your-work`/browser, and any future
skill — is the same kind of thing: a content-addressed squashfs payload plus a
self-describing `mount.json`. There is **no special "bundle" category and no
composition step**. A session mounts each selected skill as its **own** drive, swapped
into a reserved slot in the paused restore window. The existing static `skills` and
`browser` bundles fold into this as the first catalog entries.

This deliberately rejects the earlier composite/standalone split (see Alternatives):
treating the browser as a different *kind* of thing than a skill was a smell — it is just
a skill whose payload happens to be large. Under the uniform model nothing keys on a
skill's identity or size; the browser is one catalog entry on one drive like everything
else, and a future large skill needs **zero** new code.

### 1. A skill is a content-addressed mount with a manifest

Generalize `AuxRoDrive` (`engram-core/src/types/sandbox.rs`) — today
`{ drive_id, guest_mount, fs_type, sha256 }` with hardcoded `::skills()` / `::playwright()`
constructors and guest-side identification by content marker (the init shim and
`activate()` probe for `bin/engram-share` / `bin/playwright-cli`).

A skill mount is the same content-addressed RO squashfs **plus a `mount.json`** baked into
the squashfs root, e.g.:

```json
{ "kind": "skill", "skills": ["show-your-work"],
  "path_wiring": ["bin/playwright-cli"], "min_memory_mib": 1024 }
```

The guest stops sniffing for known binaries and instead **reads `mount.json`** from each
mounted drive to learn what to wire. `min_memory_mib` lets any heavy skill declare its
own floor (replacing the browser-specific 1 GiB special-case — §7/§9). Unused reserved
slots carry a tiny **sentinel** squashfs (`"kind":"sentinel"`) the guest skips.

### 2. Reserved slots in the base snapshot — small N (~12, FC-mmio-bounded)

Because FC needs every drive present at `load_snapshot`, base-snapshot capture attaches a
**fixed pool of placeholder slots** — `dyn-0 .. dyn-{N-1}`, each pointing at the sentinel
squashfs. At fresh-create the resolver `patch_drive`s the slots a session needs from
sentinel to the selected skill; unused slots stay sentinel and the guest ignores them.

**N is small (~12), set by Firecracker's device ceiling — not a preference.** engrams boots
FC `pci=off`, so every virtio device (drives included) uses the x86 **virtio-mmio** legacy
interrupt pool, `GSI_LEGACY_START=5 .. GSI_LEGACY_END=23` (19 lines). Minus the baseline
virtio devices (rootfs, net, vsock) that leaves room for roughly **~13 aux drives**, NOT the
hundreds a `pci=on` MSI pool would give. (An earlier draft proposed N=64 as a stress test;
source research on the vendored FC fork settled it before any code was written — see the
measurement gate.) The exact ceiling is pinned by a dev-vm probe; 12 leaves headroom. A
present-but-*unmounted* slot is near-free at restore (a cheap file-open + a small device
descriptor in `state.bin`, no guest re-enumeration). A profile needing more skills than fit
is the documented fallback to size-aware packing (Alternatives).

### 3. One skill = one drive — no composition, no categories

The resolver maps each selected skill to one reserved slot and swaps its content-addressed
squashfs in directly. Consequences, all good:

- **Perfect per-skill dedup.** Each skill's bytes are exactly one shared blob in
  BlobStorage and one staged file per host, referenced by every session that selects it —
  the browser's ~300 MB lives once, not duplicated into per-session composites.
- **No re-pack, no build-on-create.** Selecting/deselecting a skill changes *which slots
  are swapped*, never rebuilds anything. There is no per-set composite to build, cache, or
  stage.
- **Uniform treatment + trivial extensibility.** Browser is a catalog entry on its own
  drive like any skill; a future large skill is handled identically with no new code path.
- **The only bound is ~N skills per session.** A curated profile mounts a handful of
  skills, so the ~12 ceiling is a non-issue in practice. A profile that genuinely needs
  more is the documented trigger for size-aware slot packing (Generalization B, in
  Alternatives) — a fallback, not the default.

### 4. Late-bind in the paused restore window (the fast-boot core)

Reuse ADR 0035 §3, generalized from "swap to current generation" to "swap each selected
skill into a slot":

1. `restore_base_for_session` loads the base snapshot **paused** (already does when a swap
   plan is non-empty).
2. The resolver's plan maps each selected skill → a reserved slot; the host `patch_drive`s
   `dyn-i` from sentinel to the skill's staged squashfs.
3. `resume`.
4. agentd's bind-time **remount** (`engram-agentd/src/remount.rs`, generalized from the
   fixed `BUNDLE_MOUNTS` list to "every `dyn-*` slot") umounts the sentinel and mounts the
   swapped device at a per-slot path; `activate()` (§7) reads each `mount.json` and wires
   the declared skills.

Cost on the boot path: **O(selected-skills) `patch_drive` calls (µs each) + a remount
(~ms)** — off the UFFD critical path, identical in shape to today's generation swap.
**Pre-staging makes it a pure local-file swap:** the host-agent supervisor (ADR 0035 §5,
the `live_bundles` watch channel) already materializes pinned generations; extend it to
keep the **catalog** staged so a selected skill's bytes are local before `patch_drive`
(zero fetch on the boot path; a genuinely cold skill materializes from BlobStorage like
any pinned generation — the rare path, ADR 0035 §4).

Resume of an evicted session is **unchanged from ADR 0035**: live fds hold the mounts, no
swap, the session keeps exactly the skills it had (its eviction snapshot pinned them in
`aux_bundles`).

### 5. The catalog: coordinator owns artifacts, orchestrator owns curation

Skills have two faces, split along the existing tier boundary (ADR 0051/0053):

- **Coordinator owns the durable artifacts + a catalog of them** — each skill's
  content-addressed squashfs in BlobStorage (packed once, at registration), GC pinning,
  materialize. Natural extension of the bundle engine; mirrors `enabled_images`. The
  control plane stays **mount-aware but skills-/profile-agnostic**: it understands "a
  content-addressed RO mount at a guest path," not "a profile selected skill X."
- **Orchestrator owns curation** — the human-facing catalog metadata (name, description,
  owner) and the **profile → skills** selection, exactly as it owns profiles and
  references the coordinator's image catalog by id (ADR 0053 §3). A profile gains a
  selected-skills list, validated against the catalog at profile-save (like `image_id`).

A small **`MountCatalogService`** (app-gRPC, like `ImageService`) lets the orchestrator
list the catalog (populate the profile editor + validate). Because there is no
composition, there is no per-set build RPC — registration packs one skill's payload into
a content-addressed squashfs, and that's the whole artifact lifecycle.

### 6. Profile selection and the create-time resolve

Mirror ADR 0053's image flow (orchestrator resolves `image_id` → current `image_uri`,
passes the uri; the control plane stays dumb):

1. The profile stores selected skill **ids** (logical catalog refs), not shas — so catalog
   refreshes flow through automatically.
2. At session create, the orchestrator resolves selected ids → **per-skill mount refs** and
   passes them in a **new `CreateSessionRequest` field** (`repeated DynamicMount mounts`,
   `engram/app/v1/session.proto`). Each `DynamicMount` is `{ drive_id?, sha256,
   guest_mount, fs_type }` — opaque-to-profiles, understood by the bundle engine.
3. The coordinator's mount resolver (extends `cold_boot_spec`,
   `engram-coordinator/src/api/sessions.rs`) assigns each to a reserved slot, raises the
   memory floor to `max(min_memory_mib)` over the resolved set, and produces the swap plan
   for §4.

### 7. Activation: manifest-driven, generalized

`engram-session-bundles::activate()` stops hardcoding `share-file` /
`create-pull-request` / `show-your-work` against bundle-presence probes. It **enumerates
mounted `dyn-*` drives, reads each `mount.json`, and wires the skills + PATH entries it
declares** via the existing `wire_skill` symlink mechanism. State-dependent gating stays
(e.g. `create-pull-request` still requires `ENGRAM_FORGE_TOKEN`; `/etc/gitconfig` still
needs the forge creds) but keys off the manifest's declared requirements, not a baked-in
`if skills_bundle.exists()`. Activation stays best-effort in-guest (a mount that fails to
materialize logs + skips), with the per-skill **hard-fail moved to create-time
resolution**: a *selected* skill whose artifact can't be resolved fails the create loudly
rather than silently launching a profile that promised it.

### 8. GC: catalog pin + snapshot pin

ADR 0035 pins a generation iff a `snapshots.aux_bundles` row references it — still true for
**in-flight** mounts (an eviction snapshot pins the session's skills). Add a **catalog pin
set** so a registered-but-currently-unused skill stays offerable: an artifact is live iff
referenced by `snapshots.aux_bundles` **or** by the catalog. The heartbeat ack's
`live_bundles` becomes `snapshot-pins ∪ catalog-pins`, kept **never `serde(default)`** (the
ADR 0035 anti-sweep wire-contract invariant). With no composites, there are no per-set
blobs to track — GC is per-skill, simpler than the composite design would have been.

### 9. Migrating the existing skills/browser into the catalog

The clean break that makes this "one system": the built-in skills and the browser bundle
**stop being baked into the FC-host image as fixed always-on/opt-in drives** and become
**catalog skills**:

- `share-file` / `create-pull-request` → catalog skills, in every profile's default
  selection (the upload token is every-image; forge wiring still gates on the token at
  activation).
- `show-your-work` / browser → a catalog skill whose `mount.json` declares its
  `playwright-cli` PATH wiring and `min_memory_mib: 1024` — replacing both the
  `[browser] enabled` image flag *and* the browser-specific memory-floor special-case.

`[browser] enabled`, `AuxRoDrive::skills()`/`::playwright()`, the baked
`skills.squashfs`/`playwright.squashfs` at fixed host paths, and the init-shim content
probes all retire. Zero-users clean break; base snapshots re-bake with the reserved slots.

## Why this preserves fast boot

- Base snapshot captured **once per image** with N sentinel slots — **skill-agnostic**; no
  per-skill-combination capture.
- Selected skills bind in the **per-session paused window** that already exists, via the
  **same `patch_drive` mechanism** ADR 0035 ships, at **µs+ms cost** — off the UFFD path.
- The catalog is **pre-staged on every host**, so the swap is a local-file operation — no
  fetch on the boot path.
- No composition → **no build on the create path**, ever.
- Reserved slots are cheap at restore (a present-but-unmounted virtio-blk device backed by
  one shared sentinel); the dev-vm probe confirms the cost and pins the exact small N (next).

## Measurement gate (must run before the base re-bake)

One empirical number gates the base-snapshot re-bake. Source research on the vendored FC
fork already settled the *order*: with `pci=off`, x86 virtio-mmio caps total devices at the
19-line legacy GSI pool (`GSI_LEGACY_START=5 .. GSI_LEGACY_END=23`), so ~13 aux drives —
N=64 was never possible on the production transport. The dev-vm probe pins the rest before
we re-bake:

- **Exact aux-drive ceiling.** Attach N reserved drives and boot + snapshot + restore until
  it breaks; set `RESERVED_SLOTS` to the largest that survives with headroom (target ~12).
- **Per-reserved-slot restore cost.** Restore latency at N=0 vs the chosen N via the OTel
  spans (`fc.restore_in_jail`, `fc.spawn_uffd_handler`) — confirm ~12 empty slots don't move
  restore p50/p99 (expected: a present-but-unmounted device is near-free).

If a realistic profile ever needs more skills than the ceiling allows, the fallback is
size-aware slot packing (Generalization B) — never a per-identity special-case. Lifting the
ceiling via `pci=on` is rejected (Alternatives): it rewrites the device transport for every
VM and is a separate snapshot/UFFD-compat project.

## Phasing

- **P1 — engine generalization + admin-skill migration (one big PR is fine).** Generalize
  `AuxRoDrive` + `mount.json`; reserved slots at capture (`RESERVED_SLOTS` ~12, pinned by
  the dev-vm probe); the coordinator mount
  resolver; generalize the init shim + `activate()` to manifest-driven; catalog +
  `MountCatalogService` (admin/CI-curated entries); profile selected-skills field +
  create-time resolve; catalog pin set; migrate `share-file` / `create-pull-request` /
  `show-your-work` into the catalog and retire `[browser] enabled` + the baked bundles;
  base-snapshot re-bake. Run the measurement gate on dev-vm (slot cost + FC device
  ceiling) and validate a same-base-snapshot boot across two skill selections.
- **P2 — user-uploaded skills.** Upload (orchestrator) → deterministic pack →
  content-address → register in the catalog, **per-user/owner scoped** (ADR 0031
  Principal); upload-path GC. The artifact/pin machinery from P1 carries it; P2 adds the
  upload UX, storage, and authorization. (No composition to extend — a uniform win.)

## Consequences / risks

- **Base-snapshot re-bake.** Reserved slots change the captured device model; all base
  snapshots re-capture. Acceptable (zero users; clean break) and auto-triggered by the
  host-image roll. Growing/shrinking N later is another re-bake.
- **A small per-session skill cap (~N) exists**, set by FC's x86 mmio device ceiling
  (~13 aux drives, `pci=off`) — not a preference. Academic for curated profiles (which mount
  a handful), but real: a profile selecting > N skills is the documented trigger for
  size-aware packing (Generalization B). The fallback is designed-for, not a rewrite — the
  catalog/resolver/activation stay; only slot assignment changes.
- **FC-only.** Aux drives are unsupported on VZ (`engram-sandbox-vz` →
  `aux_ro_drives: Vec::new()`); dynamic skills ride FC in prod and ProcessBackend in dev
  (which materializes bundles from `var/bundles/` and ignores `aux_ro_drives`). VZ parity
  is a pre-existing, separately-tracked gap.
- **glibc only** for binary-bearing skills (browser); markdown/script skills unaffected
  (ADR 0027).
- **Create-time hard-fail surface.** A selected-but-unresolvable skill now fails the create
  (vs. ADR 0027's silent activation degrade) — intentional, but a new UI failure mode
  alongside ADR 0053's "image disabled out from under a profile."

## Alternatives rejected

- **Composite / size-aware slot packing (Generalization B).** Coalesce small skills into
  one shared composite slot and give large-payload skills their own slot, keyed on payload
  *size* (never identity). Removes the per-skill cap with a small N, but reintroduces a
  per-set composite build (fetch members → union → squashfs → publish), worse dedup of
  large members, and a packing heuristic. **Not chosen, but explicitly retained as the
  fallback the measurement gate may force** — and notably it is the *uniform, size-keyed*
  version, never the original "browser is a different kind of thing" split, which was a
  smell and is dropped entirely.
- **Enable virtio-PCI in FC (`pci=on`).** Lifts the device ceiling from ~13 (x86 mmio
  legacy GSIs) to thousands (MSI / `KVM_MAX_IRQ_ROUTES=4096`), so one-skill-per-drive could
  scale to N=64+. Rejected: it switches the virtio transport for *every* VM and makes PCI
  snapshot/restore/UFFD compatibility its own large validation project on an incident-prone
  substrate — far too much blast radius to lift a skills cap that packing already addresses.
- **vsock-fetch into a writable overlay** (agentd fetches payloads over the upload/forge
  vsock seam at bind, into a tmpfs/overlay — ADR 0027's "light/dynamic" sketch). No
  re-bake, no slot cap, all backends — but doesn't reuse the content-addressed RO-bundle
  engine, writes into a mutable fs (no RO immutability/dedup without re-implementing it),
  and the user chose to extend the existing engine. Retained as the natural future path for
  genuinely unbounded user-uploaded mounts if the N cap ever bites.
- **Superset-at-capture** (one "library" squashfs of the whole catalog mounted always,
  `activate()` selects a subset). One drive, no cap — but every host stages the entire
  catalog, every VM mounts it, it can't isolate user-uploaded skills, and it grows
  unbounded. Reserved-slot + per-session swap is the refinement that fixes all three.
- **Bake selected skills into the image / capture per selection.** Explodes base-snapshot
  count per skill-combination and forces cold boots — the exact failure mode this ADR
  exists to avoid.

## Deliberately deferred

- **User-uploaded skills** beyond the catalog machinery (upload UX/storage/authz) — P2.
- **Non-skill dynamic mounts** (datasets, shared caches) — the abstraction is general ("a
  content-addressed RO directory at a guest path"); only skills are wired in v1, but a
  dataset would be just another large catalog entry under the uniform model.
- **Per-session selection beyond the profile** (a user tweaking the profile's set at create
  time) — the resolve handles arbitrary sets already; the UX is profile-scoped for now
  (consistent with ADR 0053).
- **Writable / RW mounts** — everything here is read-only.

## Open questions (to settle during P1)

- **The measurement gate (above)** — the dev-vm probe pins the exact `RESERVED_SLOTS`
  (~12) under FC's x86 mmio ceiling and confirms the per-slot restore cost, before the base
  re-bake.
- **Catalog source-of-truth boundary** — confirm coordinator-owns-artifacts /
  orchestrator-owns-curation and the exact `MountCatalogService` shape (this ADR's
  recommendation; sharpen against the ADR 0053 image-catalog precedent).
- **Per-slot guest mount path** — generic `/opt/engram/dyn/<slot>` (slot-assigned, simplest)
  vs a `mount.json`-declared stable path; activation wiring is relative either way.

## Implications

- **Proto / contract:** `engram/app/v1/session.proto` `CreateSessionRequest` gains
  `repeated DynamicMount mounts`; a new orchestrator-facing `MountCatalogService`; the
  orchestrator-native `profile.proto` gains a selected-skills field. The control-plane
  `CreateSession` contract is otherwise unchanged and stays profile-agnostic.
- **Core / FC:** `AuxRoDrive` → general skill mount + `mount.json`; capture attaches
  `RESERVED_SLOTS` (~12) sentinel slots; restore extends the `fc.swap_aux_bundles` plan to
  per-session selection;
  agentd remount + `activate()` go manifest-driven; memory floor keys off the resolved set.
- **Coordinator:** mount resolver in `cold_boot_spec`; catalog table + `MountCatalogService`;
  per-skill pack + publish at registration; catalog pin set folded into the heartbeat
  `live_bundles`; a new migration (check the `deploy/migrations/` high-water mark).
- **Orchestrator:** profile selected-skills column + validation; create-time resolve in
  `CreateTask`; a Drizzle migration (check the `orchestrator/drizzle/` high-water mark).
- **Host / deploy:** the supervisor pre-stages the catalog; `deploy/bundles/*` recipes feed
  the catalog instead of fixed host paths; base snapshots re-bake with the reserved slots.
- **New failure modes for the UI:** "selected skill unresolvable at create" and (P2)
  upload/validation errors.
- **Migration / rollout:** clean break — merge auto-rolls coord + FC-host MIG, then active
  images re-enable to capture reserved-slot base snapshots, and profiles' default skill
  sets seed the catalog. The old `[browser] enabled` + baked-skills path is removed, not
  left as a fallback.
