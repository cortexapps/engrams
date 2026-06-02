# ADR 0027: Read-only host-mounted shared bundle engine — Playwright MCP + central built-in skills

Status: 2026-06-01 — **Implemented + dev-vm-validated on branch
`adr-0027-ro-mount-bundle-engine`; pending full-session e2e + prod roll.**
Full implementation landed (commit chain below); macOS `just check` green
(893 tests). **Dev-vm validated (Linux + KVM):**
- Linux `cargo clippy --workspace --all-targets -- -D warnings` clean (the
  FC-gated paths + `aux_ro_drive.rs` body that macOS clippy can't see).
- `aux_ro_drive` FC integration test — both variants pass on real microVMs
  (44.9s): RO drive re-anchors by presence across snapshot/restore, and a
  symlink-roll serves new bytes to a restored VM.
- Real glibc playwright bundle builds (`build.sh`, ~528 MB) and
  chromium-headless-shell **runs (exit 0) in a clean `debian:bookworm-slim`
  guest** with only the bundled libs/fonts. Confirmed the glibc-floor
  limitation empirically: it segfaults on the Ubuntu-22.04 dev-VM *host*
  (glibc 2.35 < the bundle's bookworm 2.36) — so the bundle requires a guest
  glibc ≥ its build base. Bundling fonts + fontconfig (a fix found here)
  cleared chromium's `Fontconfig error` so screenshots render real text.

**Still pending before Accepted:** (a) full in-session e2e — a real
`[browser] enabled` session boot where the init shim mounts the squashfs,
agentd wires the MCP, and a navigate→screenshot→`engram-share` loop lands
media on the event stream; (b) the engrams-internal half (dispatch handlers
+ FC-host packer staging `/var/lib/engram/shared/*.squashfs`); (c) prod roll
(re-bake FC-host snapshots). _Original proposal below._

## Implementation notes / divergences from the proposal

- **Activation owner is a new crate, `engram-session-bundles`** (std-only),
  not a module stuffed into agentd or engram-core (which is I/O-free by
  charter). Both agentd (FC, `root=/`) and the dev ProcessBackend
  (`root=<cwd>`) call its `activate(root, env)`.
- **Forge-token gating fix.** `ENGRAM_FORGE_TOKEN` rides `AgentSpec.env`
  (per-spawn), NOT `session_env` (coord deliberately keeps it out of the
  cached env). Activation therefore gates `create-pull-request` on the
  **union** of `session_env` + the per-spawn env. Caught by the e2e test.
- **No cold boot at session create** (ADR 0020) → aux drives attach at
  base-snapshot capture (`enabled_images`), embedded in `state.bin`, and the
  `[browser]` opt-in is an image-manifest flag (not a session param). Memory
  floor is a shared `resolved_memory_mib` helper so capture + restore can't
  drift (FC requires equal `mem_size_mib`).
- **Restore re-anchors by presence, no `patch_drive`** — bundle paths are
  fleet-canonical stable symlinks. `restore_canonical_symlinks` asserts
  presence (clear `SandboxError::Snapshot`) instead of an opaque FC error.
- **Memory** is a non-issue at the 4 GiB default (the proposal worried it
  might be ~256 MiB); the 1 GiB browser floor only bites images that lowered
  `suggested_memory_mib`.
- **Init-shim mount** marker-probes `/dev/vdb..vde` (squashfs-only, by
  content marker) rather than hard-coding a device letter — order- and
  CA-drive-independent.
- **CI**: a `bundles` lane publishes both squashfs to GHCR
  (`bundle-{skills,playwright}`) + fires `engrams-bundles-changed`; a
  `dev_image` lane fires `engrams-dev-image-changed` to auto-rebake the
  dogfood image. Enable stays manual.

Commit chain (branch `adr-0027-ro-mount-bundle-engine`):

```
dcc8016 docs(adr): 0027 … Proposed
d22de3a feat(core): AuxRoDrive + SandboxSpec.aux_ro_drives
7da7302 feat(firecracker): attach aux RO drives + re-anchor by presence
58cb86f feat(image-builder): mount RO bundles in the init shim
52a0a78 feat(session-bundles): new crate — activate bundles in agentd
e7b9396 refactor(image-builder)!: retire baked skill injectors
e85216d feat(coordinator): [browser] flag + capture wiring + memory floor
a12364a feat(sandbox-process): dev parity
cc8b549 build(bundles): build recipes + manifests + CI publish + just bundles
20251a4 ci: auto-rebake dev-engrams on source changes
9cef930 test(session-bundles): e2e + forge-gating fix
```

## Validation status

Done (dev-vm, Linux + KVM):
- [x] `aux_ro_drive` FC integration test — both variants pass on real microVMs.
- [x] Linux clippy `-D warnings` clean (FC-gated paths).
- [x] Real glibc playwright bundle builds; chromium-headless-shell runs
      (exit 0) in a clean `debian:bookworm-slim` guest, fonts render.

Pending (before Accepted):
1. Full in-session e2e: boot a `[browser] enabled` session, confirm the init
   shim mounts both bundles, agentd wires the playwright MCP + skills, and a
   `browser_navigate` + `browser_take_screenshot` → `engram-share` loop lands
   media on the event stream. Confirm `share-file` / `create-pull-request`
   still work on a non-browser git image (post-retirement). This needs a
   baked `[browser]` image + the bundles staged at the canonical host path +
   a running stack — i.e. it rides on (2).
2. engrams-internal (separate repo): add the `engrams-bundles-changed` +
   `engrams-dev-image-changed` `repository_dispatch` handlers and the FC-host
   packer step that stages `/var/lib/engram/shared/*.squashfs`.
3. Prod roll (re-bake FC-host snapshots so the bundles are present at the
   canonical paths the new base snapshots embed).

ADR 0026 gave agents a way to *surface* images/videos in the session conversation
(`engram-share`); it closed by naming the next step: "The end goal is browser tooling
(Playwright) for in-session agents." This ADR delivers that — and, in doing so, builds
the general delivery mechanism ADR 0023 deferred.

We give in-session agents the official **`@playwright/mcp`** server backed by
**`chromium-headless-shell`**, so an agent can drive a browser against its own work (a
dev server on the guest's localhost), take a screenshot or record a WebM, and surface
it with the existing `engram-share` (ADR 0026 already accepts `image/png` + `video/webm`
— no change there). The browser **must** run inside the guest: the only thing that can
reach the agent's `http://localhost:<port>` dev server is the guest itself.

The design question is *delivery*, not capability. Baking Chromium into every image is
wrong on three counts: session images are **user-authored Dockerfiles** (the platform
can't assume the base carries glibc + the ~20 system libs Chromium needs), the browser
is **+300–500 MB** of dead weight on images that never open it, and the platform only
injects glue *post-build* — it can't `apt-install` browser deps into an arbitrary
rootfs. So the browser lives outside the image, mounted in.

This is exactly the **RO-mount skills/MCP engine ADR 0023 deferred**. Playwright is the
forcing function; the engine is general. To prove it generalizes — and to retire code —
we make the existing built-in skills (`share-file`, `create-pull-request`) the engine's
**second consumer**, moving them off bake-time rootfs injection.

## Context

- ADR 0023 deferred "the dynamic, per-session, RO-mounted, centrally-managed skills
  platform" and recorded *why it isn't a quick "mount it" on FC*: the per-session
  harness *drive* was retired in ADR 0021 P1.5 (the harness moved into the rootfs), and
  **Firecracker has no virtiofs** (its device model is virtio-blk/net/vsock/rng/balloon
  only). So a per-session RO mount necessarily means an **additional read-only
  virtio-blk drive** (`put_drive` with `is_read_only`), mounted guest-side, and
  **re-anchored on restore** (the snapshot embeds the host path) — reviving the drive
  machinery 0021 removed.
- That machinery is intact and tested: `DriveConfig { is_read_only }`, `put_drive`,
  `patch_drive` (PATCH on a paused VM, re-kicks the virtio-blk queue on resume —
  `crates/engram-sandbox-firecracker/tests/patch_drive_swap.rs`), and the harness-drive
  re-anchor-on-resume work (ADR 0018 §12p). The init shim already RO-mounts `/dev/vdb`
  to stage the egress CA — the guest-side mount pattern.
- **There is no cold boot at session create** (ADR 0020): create *always* restores. The
  only cold boot per image is `capture_and_record_base_snapshot` at image-enable
  (`crates/engram-coordinator/src/api/enabled_images.rs`). A drive must therefore be
  attached **during base-snapshot capture** to be embedded in `state.bin`; opt-in must
  be known at enable time → it is an **image-manifest flag**, not a session param.
- Built-in skills today are baked into the rootfs: `inject_share_helpers` (every image)
  and `inject_forge_helpers` (`[git]` images) write thin wrapper scripts + `SKILL.md` +
  `/etc/gitconfig` and symlink `/root/.claude/skills → /root/.agents/skills`
  (`crates/engram-image-builder/src/lib.rs`). The wrappers are one-liners over
  `engram-agentd` subcommands (`share-file`, `forge-pull-request`, `forge-credential`),
  and `engram-agentd` is already in the rootfs — so the baked glue is non-load-bearing.
- `engram-agentd` owns the harness launch and the durable session env (applied to the
  harness, `/exec`, and the ttyd shell) from `AgentSpec.session_env` on the
  `SpawnHarness` frame (`crates/engram-agentd/src/harness_supervisor.rs`). It runs per
  resume and can write the rootfs.
- `SandboxSpec` is the snapshot-portable unit (`FcSnapshotManifest.spec`); a field added
  there survives restore + reattach by construction.

## Decision

A general **read-only host-mounted shared bundle engine**. Bundles are self-contained,
content-addressed squashfs images, shipped as **fleet-wide FC-host assets** at canonical
stable-symlink paths, and attached to guests as additional read-only virtio-blk drives.
Two bundles, two consumers:

- **`skills.squashfs` — always attached.** The relocated `engram-share` / `engram-pr` /
  `git-askpass` wrappers and the `share-file` / `create-pull-request` `SKILL.md` dirs.
  Tiny; the value is fleet-wide skill updates with **no per-image re-bake**.
- **`playwright.squashfs` — opt-in (`[browser] enabled`).** `chromium-headless-shell` +
  a pinned Node runtime + `@playwright/mcp` + all `.so` deps + a `launch-mcp` launcher +
  a `record-demo` skill. ~80–100 MB.

**Fleet-canonical path, not per-sandbox.** Unlike the per-sandbox rootfs (which needs
the id-keyed canonical-symlink indirection so a cross-host restore re-points cleanly),
a bundle is byte-identical on every host. We attach it at a stable symlink
(`/var/lib/engram/shared/<name>.squashfs → <name>-<sha>.squashfs`). The snapshot embeds
that stable path; every host has the same file there; **restore re-anchors by mere
presence — no `patch_drive`, no page-cache-invalidation subtlety.** A version roll moves
the symlink target; existing snapshots keep working because they reference the symlink,
not the resolved sha.

**Activation is dynamic, per-session, in agentd — not baked.** The bundles are static
fleet-wide; *which* skills/MCP are wired is decided at `SpawnHarness`, before the harness
launches, from session env + which bundles mounted:

- `share-file` — always (the upload token is every-image per ADR 0026).
- `create-pull-request` + `/etc/gitconfig` — iff forge env present (`ENGRAM_FORGE_TOKEN`).
- `record-demo` + `/root/.mcp.json` (`mcpServers.playwright → launch-mcp`) — iff the
  browser bundle mounted.

This is what makes built-in skills *dynamic* (the ADR-0023 goal): editing a skill ships
fleet-wide by rolling the bundle, no image re-bake. It also replaces, rather than adds
to, the bake-time injectors.

**Opt-in + memory.** `[browser] enabled = true` in the image manifest pushes the
playwright `AuxRoDrive` into the base-snapshot capture spec and raises the memory floor
to 1024 MiB (headless shell needs ~250–400 MB; the 4096 MiB default already covers it, so
this only bites images that lowered `suggested_memory_mib`). The skills drive is pushed
unconditionally.

## Consequences / risks

- **Snapshot embeds the drive path** → every FC-host image must carry the bundle at the
  canonical symlink. Mitigated by the stable symlink + a **presence assertion** on
  restore (clear `SandboxError::Snapshot` instead of an opaque FC virtio error) + a
  packer invariant documented for the deploy repo.
- **Universal skill delivery now depends on the engine.** A missing `skills.squashfs`
  could otherwise brick *every* session, not just browser ones. Mitigated by
  **agentd graceful-degrade**: an absent bundle logs and skips the skill — it never fails
  the session. Skills are best-effort.
- **glibc only.** The bundle is glibc-linked; **musl/alpine base images cannot run it**
  (the mount succeeds, the binaries don't). Documented as a hard limitation; the baker
  warns on `[browser] enabled` against a musl base.
- **Mount latency** — one squashfs RO mount at init (~ms, page-cache friendly), captured
  in the snapshot, so resume pays nothing.
- **Security.** Read-only drives, no new privileged guest capability, no new coord
  surface. The MCP server is a child of the harness at the session's privilege
  (consistent with ADR-0023's "skills run at session privilege") and reaches only guest
  localhost + whatever the egress proxy `allow_hosts` permits. Note `browser_evaluate` /
  `browser_run_code_unsafe` execute arbitrary in-page JS — the same trust boundary 0023
  set for skills, acceptable for single-org tenants.

## Alternatives rejected

- **Bake Chromium into every image** — base-image-dependent (glibc + system libs), +500
  MB of dead weight, and impossible via post-build injection. (See intro.)
- **Bake into the dogfood image only** — proves the loop but never generalizes; leaves
  the MCP wiring as a one-off and doesn't retire the skill injectors.
- **Host-side / remote browser over CDP** — can't reach the agent's guest-localhost dev
  server, defeating the primary use case.
- **Digest-pinned drive path + `patch_drive` on restore** — works (it's the tested
  harness-drive mechanism) but needlessly revives the page-cache-invalidation subtlety;
  the stable symlink makes re-anchor a presence check.

## Implementation phases

1. ADR 0027 (Proposed — this doc).
2. `AuxRoDrive` + `SandboxSpec.aux_ro_drives` (serde default) + round-trip tests.
3. FC attach on cold create + restore presence-assert + `tests/aux_ro_drive.rs` (wired
   into `ci.yml` `test-firecracker`).
4. Init-shim guest mount (skills + browser, marker-probe).
5. agentd activation (selective skill symlinks + gitconfig + `.mcp.json` + bundle env;
   graceful-degrade).
6. Retire `inject_share_helpers` / `inject_forge_helpers`; relocate the `SHARE_*` /
   `FORGE_*` constants to `deploy/bundles/skills/`; update builder tests.
7. `BrowserConfig` manifest flag + capture-spec wiring (skills always, browser opt-in) +
   memory floor + musl warn.
8. ProcessBackend dev parity (symlink bundle dirs) + `just bundles`.
9. `deploy/bundles/{skills,playwright}` build recipes + manifests + CI publish lane.
10. CI `dev_image` lane + `engrams-dev-image-changed` dispatch (auto-rebake the dogfood
    session image on engrams source changes; **enable stays manual**).
11. ADR 0027 → Accepted (commit chain + divergences).

Out of scope (still deferred from ADR 0023): user-uploaded skills (registry + upload
API/UI + PG `skills` table + org enable/disable). This ADR delivers only the platform-
curated RO-bundle engine; user uploads ride it later.
