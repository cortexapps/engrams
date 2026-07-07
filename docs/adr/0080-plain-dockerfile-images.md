# 0080 — Plain-Dockerfile session images: dynamic agentd, out-of-band ImageConfig, host-side materialization

Status: Proposed (2026-07-07)

Builds on: ADR 0036 (per-chunk artifacts + async enable), ADR 0055
(uniform dynamic mounts), ADR 0057 (unified session policy — the half
that stripped `[secrets]`/`[network]` from the manifest), ADR 0062
(built-in harness via the bundle stamp — the delivery precedent agentd
copies), ADR 0025 (owning the guest kernel — the "engrams artifact
outside the image" precedent), ADR 0078 (tier-0 disk veto — reused for
materialize host picking).

## Problem

A session image bake couples four independently-evolving things into
one slow artifact build:

1. **Metadata** — `engram.toml`'s runtime half (name, description, env,
   workdir, resources, `[warm]`) is rendered into a `manifest.toml`
   layer and only ever *consumed* from Postgres (`enabled_images.
   manifest_toml`) at enable/session-create time. Changing a
   description or a warm timeout re-runs a bake that can take ~25
   minutes (dev-brain, gradle-bound).
2. **engrams-owned guest binaries** — `engram-agentd` + the
   `/sbin/engram-init` shim are injected into the rootfs by
   `engram-image-builder`. Any agentd wire change forces a full
   re-bake of every image (`detect-rebake-lanes.py`:
   `cli_tools → dev_image`). This is what currently wedges dev-brain:
   a breaking agentd change landed and the image can't be re-baked.
3. **The pack/chunk/push pipeline** — `docker export` → mke2fs →
   chunk → OCI push all runs inside `engram-image-builder`, so users
   can't bring a plain docker image.
4. **Warm capture options are half-migrated** — `capture_env` (warm
   secrets) already rides the EnableImage RPC but ad-hoc-ly: no
   UpdateImage verb (edits overload EnableImage), the CLI can't set it,
   secret refs resolve with an empty `SecretSchema` and **fail soft**
   (warn + skip), and the web form's ref field is free text. Meanwhile
   `warm.network` (capture egress) is still frozen in the baked
   manifest — the one network knob ADR 0057 never relocated — and the
   `SessionEgressPolicy` for capture is built by a *second* builder in
   the host-agent (`capture_egress_policy`) instead of the
   coordinator's `assemble_egress_policy`.

## Decision

**End state: a session image is a plain `docker build && docker push`
of a standard OCI image to any registry.** Everything else moves to
where it can change cheaply:

| Concern | Old home | New home | Change cost |
|---|---|---|---|
| name/description | baked manifest | `image_config` (RPC) | immediate |
| env/workdir | baked manifest | `image_config` (RPC) | next session create |
| resources | baked manifest | `image_config` (RPC) | recapture |
| warm command/timeout/workdir | baked manifest | `image_config.warm` (RPC) | recapture |
| warm secrets | `capture_env` side-channel | `image_config.warm.env` | recapture |
| warm egress | baked manifest `[warm.network]` | `image_config.warm.network` | recapture |
| agentd + init behavior | baked into rootfs | `bundle-agentd` (fleet stamp) | **zero recapture** |
| ttyd | Dockerfile COPY | `guest-tools` bundle | zero recapture |
| kernel | already host-staged (ADR 0025) | unchanged | — |
| harness | already `dyn_0` bundle (ADR 0062) | unchanged | — |
| OCI→ext4→chunks | bake CLI | enable-time `MaterializeImage` host RPC | per enable/rebase |
| build secrets | `[build] build_secrets` | caller's `docker build --secret` | not engrams' concern |

`engram.toml` and `engram-image-builder` are retired at the end.

### A. Dynamic agentd: a bundle slot + a tiny stage-1 init + re-exec on fresh restore

agentd is resident in every base snapshot's memory image, so *no*
delivery mechanism (initramfs included — the initrd is only read at
the one cold boot per image) can update it for restores except the one
thing the restore path already swaps: the aux RO bundle drives
(`patch_drive` in the paused load window, ADR 0055/0062).

- **Slot layout** (`engram_core::types::sandbox::AuxRoDrive`):
  `dyn_0` = harness (unchanged), **`dyn_1` = agentd
  (`AGENTD_SLOT_INDEX`, stamp key `agentd`)**, skills = `dyn_2..`
  (`MAX_SKILL_SLOTS = RESERVED_SLOTS - 2`). Breaking slot renumbering
  is sanctioned — every image is re-captured once at rollout (§Rollout).
- **`bundle-agentd`**: a reproducible squashfs (same `_pack.sh` as the
  harness) carrying `engram-agentd` + `agentd.sha256` (content stamp).
  Published by `publish-bundles`, staged by `node-assets-fetch.sh`,
  stamped in `current.json` under `agentd`. Per-arch like every bundle.
- **Stage-1 init** (the only file still delivered via the rootfs):
  today's shim minus the baked agentd. It additionally mounts a tmpfs
  on `/run`, and after mounting the bundle slots **probes them for
  `engram-agentd`** (position-independent — VZ compacts resolved
  drives onto `/dev/vdb..`), copies the binary + `agentd.sha256` to
  `/run/engram/`, and `exec`s `/run/engram/engram-agentd`. Running
  from tmpfs makes a later drive swap harmless: agentd's text pages
  are guest memory, nothing executes from the drive (the same
  asymmetry that makes the harness swap safe).
- **Capture (and any cold boot)**: the spec's agentd slot stays
  *symbolic* and the **host** resolves it against its own stamp
  (`current.json[agentd]`) — the same ownership as the sentinel
  resolution (ADR 0035: "attach whatever this host currently stages"),
  and it makes the evacuation Fix-B cold-boot recovery work with no
  coordinator changes. FC resolves it in the cold-boot attach; VZ —
  which skips unresolved (sentinel) slots — resolves the agentd slot
  the same way before its skip. A host that stages no `agentd` bundle
  cannot cold-boot — loud.
- **Session create (fresh restore)**: the coordinator appends an
  agentd `AuxRoDrive` (slot 1, sha resolved from the fleet catalog) to
  `selected_mounts`, exactly like the harness. Resolution is **soft
  with a loud warn** when no host reports an `agentd` bundle: the
  restore keeps the snapshot's pinned generation (hard-checked staged
  at restore), so a bundle-less fleet (Process dev, mid-bring-up)
  still binds sessions — it just can't roll agentd. Capture stays
  hard: an FC/VZ host that stages no agentd bundle cannot cold-boot,
  so mis-staging can't compound into new images. The host `patch_drive`s
  it in the paused window. **Latency guard (boot time is paramount):**
  the restore records whether the agentd slot's content actually
  changed vs. the snapshot's pin (`LiveSandbox.agentd_slot_swapped`);
  when it didn't — the steady state — `refresh_agent` returns without
  any guest round-trip, so the whole mechanism adds **zero** to the
  create path. Only when the slot content changed does the host send
  the new **`WireRequest::RefreshAgent`** (append-only wire enum),
  after resume and before any session state binds. agentd then:
  re-mounts the bundle mounts (the ADR 0035 §3 dance), reads the
  slot's `agentd.sha256`, compares `/run/engram/agentd.sha256`:
  - equal → replies `UpToDate` (backstop; the host normally skips).
  - differ → copies binary + stamp to `/run/engram/` (temp + rename),
    replies `Restarting`, flushes, and `execv`s itself with its own
    argv (PID 1 exec; env preserved). The host re-polls readiness with
    a tight 20 ms cadence. Cost: ~tens of ms, paid only on the first
    creates after an agentd roll.
  `refresh_agent` failures are **non-fatal** (warn + proceed with the
  captured agentd — the pre-roll behavior): a transitional old-model
  snapshot or a staging hiccup degrades to yesterday's agentd, never
  to a failed create.
- **Resume (mid-session)**: never swapped, never refreshed (existing
  behavior — live fds). **agentd version is pinned per-session at
  create.** Skew policy: host↔agentd protocol changes stay compatible
  across one version where practical; genuinely breaking changes
  terminate in-flight sessions as part of the roll (zero users).
- **Detector**: `bundles |= agentd_changed`;
  `engram-agentd` leaves `CLI_TOOLS_BINS`' `dev_image` coupling. An
  agentd merge publishes a bundle; it no longer re-bakes any image.

Recapture remains only for: image content, resources, warm config, and
(rare) stage-1 init changes.

### B. One `ImageConfig`, RPC-set, UpdateImage verb

- `ImageManifest` → `ImageConfig` (deny_unknown_fields restored):
  `name`, `description`, `env`, `workdir`, `resources`,
  `warm: Option<WarmConfig>`. **`WarmConfig` absorbs every capture
  option**: `command`, `timeout_secs`, `workdir`,
  `env: Vec<CaptureEnvEntry>` (literal | secret_ref — retires the
  standalone `capture_env` columns and request field), `network:
  NetworkPolicy`.
- New sibling `OciRuntimeDefaults { env, workdir }` extracted from the
  standard OCI image config blob (Dockerfile `ENV`/`WORKDIR`), merged
  under admin config by `ImageConfig::merged_with` (same precedence as
  today's `apply_image_config_defaults`).
- **Postgres (migration 0094)**: `enabled_images` −`manifest_toml`
  −`capture_env` +`image_config JSONB NOT NULL` +`oci_defaults JSONB
  NOT NULL`; `enable_jobs` −`capture_env` +`image_config JSONB NOT
  NULL`. Config rides the job and is stamped onto the row only at
  `ready`, so capture-affecting edits are invisible to session-create
  until the new base snapshot exists. Zero users: both tables are
  emptied first.
- **RPC**: `EnableImageRequest.config` (unset = inherit the enabled
  row's config; required on first enable). New `UpdateImage`
  (full-replace): cheap fields (name/description/env/workdir) apply
  immediately (row UPDATE + boot-bundle-cache NOTIFY); a diff touching
  `resources` or anything under `warm` requires `allow_recapture:
  true` → enqueues an enable job, else `FailedPrecondition` naming the
  fields (explicit admin trigger).
- **Warm secrets fail loud**: `resolve_capture_env` gets a real
  `SecretSchema` and an unresolved ref **fails the capture job** — a
  silently-missing secret bakes a corrupt warm snapshot.
- **One egress builder**: the coordinator assembles the capture
  `SessionEgressPolicy` through the same code path as sessions
  (`assemble_capture_egress_policy` in `session_boot.rs`, the capture
  flavor of `assemble_egress_policy`) and ships it in the
  `build_base_snapshot` request (`capture_egress_bincode`, wire v13);
  the host-side `capture_egress_policy` builder retires. **Phase 2a
  divergence**: the split mirrors issue #535(c)'s session-side split —
  the coordinator assembles the sandbox-INDEPENDENT posture half
  (allow_all / allowlists / a synthetic teardown session_id) with
  placeholder identity, and the host stamps the sandbox-DEPENDENT half
  (sandbox_id + guest IP, which only exist once the capture VM boots
  inside `build_base_snapshot`) at registration. Enforcement
  (`egress::register_policy`) is already unified and stays put.
- Snapshot-reuse key: `(disk_manifest, image_config->'resources')`
  replaces `(disk_manifest, manifest_toml)` (warm images already never
  reuse).
- Surfaces: full create/edit form in the web ImagesPanel (env rows,
  resources, warm command/timeout/env-with-OrgSecretCombobox, network
  editor mirroring the profile editor — the wire reuses the
  `ProfileNetwork` proto shape, recapture confirmation); orchestrator
  policy-map `ImageService.UpdateImage: manage/all`; CLI
  `image enable/update --config <toml> [--allow-recapture]`.
- **Phase 2a interim artifact** (retired wholesale by phase 3): the
  bake keeps producing the chunked engram artifact, but the
  `manifest.toml` layer is GONE (`ENGRAM_MANIFEST_MEDIA_TYPE` retired)
  and the OCI config blob gains `runtime_defaults` — extracted at bake
  via `docker inspect` (now a HARD bake error on failure; a silent
  fallback would strip the Dockerfile ENV/WORKDIR from every session).
  The enable pipeline fail-louds on artifacts missing
  `runtime_defaults` ("re-bake with a current builder"). `engram.toml`
  is already reduced to an OPTIONAL `[build]`-only file — a leftover
  manifest-era key fails the bake with a pointer to
  `image enable --config`. The host's image-cache "manifest.toml
  exists" sentinel is replaced by the bundle/rootfs presence check
  (nothing host-side ever read its contents).
- **Phase 2a divergence — in-flight-config guard**: re-POSTing an
  enable while a job is in flight returns that job
  (`create_or_get_enable_job`'s dedup), but the in-flight job captures
  under ITS config; if the caller supplied a DIFFERENT config the
  request fails `Conflict`/`FailedPrecondition` (admin-visible
  check-then-act) instead of silently dropping the edit.

### C. Host-side materialization

New crate `engram-rootfs-materializer`: pull a standard OCI/Docker
image (streaming, platform by host arch), whiteout-aware flatten
(`.wh.`, opaque dirs, xattrs/hardlinks/setuid, ownership), extract the
config blob → `OciRuntimeDefaults`, inject stage-1 init (one file),
mke2fs pack (deterministic: clamped mtimes + the pinned static mke2fs,
which moves into the host-agent image), chunk into the host chunk
store / BlobStorage.

New server-streaming host RPC `MaterializeImage` (one WIRE_VERSION
bump): `{image_uri, platform, resolved registry auth}` → progress
frames → `{disk_manifest_ref, oci_defaults, size}`. Sibling of
`build_base_snapshot` (same scanner lease-renewal shape). The enable
pipeline's `materialize_disk_chunks` + `fetch_and_seal_manifest`
retire; old-style engram OCI artifacts can no longer be enabled (clean
break; existing rows keep working — their chunks are already in
BlobStorage).

Host safeguards: ADR 0078 disk-veto host pick, ~2.5× image-size
scratch budget integrated with chunk-cache accounting, scrub on all
exit paths, enable-time image size cap, ≤1 concurrent materialize per
host. Registry auth: static creds resolved coordinator-side and passed
in the request; GCP workload identity resolved host-side
(`engram-oci-auth`). Per-session latency: zero — materialize is
enable/rebase-time only.

### D. Purification

ttyd → `guest-tools` bundle (agentd's `shell.rs` resolves it from the
mount); `.bashrc` written by stage-1 init if absent; image contract =
"any linux image with `/bin/sh`" (git/curl/socat/iproute2 documented
as workspace requirements). `engram-image-builder`,
`engram-cli image build`, `bake-dev-image.yml`, and the bake half of
`just bake-demo` are deleted; dev flow becomes
`docker build && docker push localhost:5001/… && engram image enable
--config …`.

## Alternatives rejected

- **Initramfs delivery for agentd** (kernel-style, ADR 0025): perf is
  fine (~10–20 MB initrd, cold-boot-only), but the initrd is read only
  at capture — restores keep the captured agentd, so every agentd roll
  still recaptures every image. Fails the requirement, and adds PID1-
  from-initramfs rework (no `/bin/sh` in ramfs), kernel-config
  verification on FC + VZ kernels, a VZ bootloader change, and an
  agentd-version dimension in the snapshot-reuse key.
- **Materialize-time injection of agentd** (no bundle): simpler guest
  logic, but rolling agentd = re-materialize + recapture every image.
  Kept only for the stage-1 init, which is tiny and ~never changes.
- **Coordinator-side OCI flatten**: needs root-only untar semantics
  (uid/gid, xattrs, device nodes) and image-sized scratch; the coord
  pod is small and unprivileged (a 7.6 GiB image once OOM-killed it).
  Hosts are privileged, have NVMe + the chunk store, and already run
  the other enable-time host job (`build_base_snapshot`).

## Phases

1. **Dynamic agentd** (this PR chain's first phase; ships via the
   existing bake path, unblocks dev-brain after one final re-bake per
   image): bundle + slot + stage-1 shim + RefreshAgent + coordinator
   resolution + detector; FC integration test `agentd_bundle_reexec`.
2. **ImageConfig + UpdateImage** (2a core+wire: types, migration 0094,
   proto, pipeline, fail-loud secrets, egress-builder consolidation,
   CLI; 2b surface: orchestrator authz + web form + e2e assertions).
3. **Materializer** (3a crate + fixture tests; 3b wire bump +
   `MaterializeImage` + pipeline switch + e2e replacement + FC test
   `materialize_and_boot`).
4. **Purification + retirement**; flip this ADR to Accepted with the
   commit chain.

## Rollout

- Phase 1 rolls coord+host together (no wire bump — RefreshAgent is an
  in-guest wire append; old-snapshot restores degrade gracefully via
  the non-fatal refresh), then one final re-bake + re-enable per image
  adopts the new slot layout/shim. From then on agentd ships by bundle
  publish.
- Phase 2's migration 0094 wipes `enabled_images`/`enable_jobs`:
  quiesce sessions, re-enable each image via the new UI/CLI (dev-brain
  pays one warm capture).
- Phase 3 requires images be pushed as standard docker images
  (dev-brain already is); enable/rebase re-materializes on a host.

## Risks

- OCI flatten correctness (whiteouts/xattrs/capabilities/hardlinks) —
  dev-vm golden-diff vs `docker export` before cutover; fail loud on
  unknown media types.
- PID-1 re-exec must strictly precede session binding — pinned by the
  FC integration test.
- Env cheap-edits assume non-warm base snapshots are env-agnostic;
  verified in Phase 2 (else env joins the reuse fingerprint).
- Host disk pressure during materialize — budget + scrub + cap + veto.
- UpdateImage vs in-flight-job race: check-then-act guard; admin-
  visible and self-healing; accepted.
