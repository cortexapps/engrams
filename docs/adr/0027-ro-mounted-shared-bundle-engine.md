# ADR 0027: Read-only host-mounted shared bundle engine — browser tooling + central built-in skills

Status: 2026-06-02 — **Accepted.** The RO-mount bundle engine + central skills
shipped in #55. The browser consumer is **`@playwright/cli`** (this revision;
an earlier draft used an MCP server — see "Why not an MCP server"). Engine +
the `@playwright/cli` capture loop are dev-vm-validated (real microVM RO-drive
re-anchor; chromium-headless-shell explore + screenshot + WebM in a clean
glibc guest). Pending: a full in-session dogfood (browser ↔ the engrams dev
server) + the prod roll.

ADR 0026 gave agents a way to *surface* images/videos in the session
conversation (`engram-share`); it closed by naming the next step: in-session
browser tooling. This ADR delivers that — and, in doing so, builds the general
delivery mechanism ADR 0023 deferred.

We give in-session agents a real headless browser (**`chromium-headless-shell`**)
driven by **`@playwright/cli`**, so an agent can navigate its own work (a dev
server on the guest's `localhost`), explore it (accessibility snapshot → click
→ observe), screenshot key states, and record a WebM — then surface them with
`engram-share` (ADR 0026 already accepts `image/png` + `video/webm`). The
browser **must** run inside the guest: only the guest can reach the agent's
`http://localhost:<port>` dev server.

The design question is *delivery*, not capability. Baking Chromium into every
image is wrong on three counts: session images are **user-authored Dockerfiles**
(the platform can't assume the base carries glibc + the ~20 system libs Chromium
needs), the browser is **+300–500 MB** of dead weight on images that never open
it, and the platform only injects glue *post-build* — it can't `apt-install`
browser deps into an arbitrary rootfs. So the browser lives outside the image,
mounted in.

This is exactly the **RO-mount skills/browser engine ADR 0023 deferred**. The
browser is the forcing function; the engine is general. To prove it generalizes
— and to retire code — we make the existing built-in skills (`share-file`,
`create-pull-request`) the engine's **second consumer**, moving them off
bake-time rootfs injection.

## Context

- ADR 0023 deferred "the dynamic, per-session, RO-mounted, centrally-managed
  skills platform" and recorded *why it isn't a quick "mount it" on FC*: the
  per-session harness *drive* was retired in ADR 0021 P1.5 (the harness moved
  into the rootfs), and **Firecracker has no virtiofs** (its device model is
  virtio-blk/net/vsock/rng/balloon only). So a per-session RO mount necessarily
  means an **additional read-only virtio-blk drive** (`put_drive` with
  `is_read_only`), mounted guest-side, and **re-anchored on restore** (the
  snapshot embeds the host path).
- **There is no cold boot at session create** (ADR 0020): create *always*
  restores. The only cold boot per image is `capture_and_record_base_snapshot`
  at image-enable (`engram-coordinator/src/api/enabled_images.rs`). A drive must
  therefore be attached **during base-snapshot capture** to be embedded in
  `state.bin`; opt-in must be known at enable time → it is an **image-manifest
  flag**, not a session param.
- Built-in skills were baked into the rootfs: `inject_share_helpers` (every
  image) + `inject_forge_helpers` (`[git]` images). The wrappers are one-liners
  over `engram-agentd` subcommands, and `engram-agentd` is already in the rootfs
  — so the baked glue is non-load-bearing and can move to a bundle.
- `engram-agentd` owns the harness launch and the durable session env (applied
  to the harness, `/exec`, and the ttyd shell) from the `SpawnHarness` frame
  (`engram-agentd/src/harness_supervisor.rs`). It runs per resume and can write
  the rootfs.
- `SandboxSpec` is the snapshot-portable unit (`FcSnapshotManifest.spec`); a
  field added there survives restore + reattach by construction.

## Decision

A general **read-only host-mounted shared bundle engine**. Bundles are
self-contained, content-addressed squashfs images, shipped as **fleet-wide
FC-host assets** at canonical paths, and attached to guests as additional
read-only virtio-blk drives. Two bundles, two consumers:

- **`skills.squashfs` — always attached.** The relocated `engram-share` /
  `engram-pr` / `git-askpass` wrappers + the `share-file` / `create-pull-request`
  `SKILL.md` dirs. Tiny; the value is fleet-wide skill updates with **no
  per-image re-bake**.
- **`playwright.squashfs` — opt-in (`[browser] enabled`).** `chromium-headless-shell`
  + a pinned Node runtime + Microsoft's **`@playwright/cli`** + all `.so` deps +
  a `playwright-cli` wrapper + the `show-your-work` skill. The wrapper bakes in
  the runtime env (`LD_LIBRARY_PATH`, `PLAYWRIGHT_MCP_CONFIG` → a config pinning
  `chromium-headless-shell` + `--no-sandbox`, fontconfig, bundle node), so the
  agent runs `playwright-cli open <url>` with **zero flags**.

**Fleet-canonical path, presence re-anchor.** Unlike the per-sandbox rootfs
(id-keyed canonical-symlink so a cross-host restore re-points cleanly), a bundle
is byte-identical on every host, baked at `/var/lib/engram/shared/<name>.squashfs`.
The snapshot embeds that path; every host has the same file there; **restore
re-anchors by mere presence — no `patch_drive`.** A bundle-content roll re-bakes
the FC-host image + rolls the MIG; existing base snapshots keep working because
the path is stable (the bytes there just change).

**Activation is dynamic, per-session, in agentd — not baked.** The bundles are
static fleet-wide; *which* skills/tools are wired is decided at `SpawnHarness`,
before the harness launches, from session env + which bundles mounted
(`engram-session-bundles::activate`, shared by FC agentd and the dev
ProcessBackend):

- `share-file` — always (the upload token is every-image per ADR 0026), iff the
  skills bundle mounted.
- `create-pull-request` + `/etc/gitconfig` — iff a forge token is present
  (`ENGRAM_FORGE_TOKEN`, which rides the per-spawn `AgentSpec.env`, so activation
  gates on the union of that + `session_env`).
- `show-your-work` + the `playwright-cli` wrapper symlinked onto PATH — iff the
  playwright bundle mounted. **No MCP config, nothing per-harness.**

Editing a skill ships fleet-wide by rolling the bundle, no image re-bake. This
replaces, rather than adds to, the bake-time injectors.

**Opt-in + memory.** `[browser] enabled = true` pushes the playwright
`AuxRoDrive` into the capture spec and raises the memory floor to 1 GiB (headless
shell needs ~250–400 MB; the 4 GiB default already covers it — a shared
`resolved_memory_mib` helper keeps capture + restore in lockstep, since FC
requires equal `mem_size_mib`). The skills drive is pushed unconditionally.

## Why not an MCP server

An earlier draft shipped `@playwright/mcp` (an MCP server the harness talks to).
The dev-vm spike showed the CLI is the better fit here:

- **MCP needs per-harness config discovery; the CLI needs none.** Claude Code
  discovers MCP servers from a project `.mcp.json` (cwd-relative, and *prompts
  for approval* — fatal headlessly), `~/.claude.json`, or a `--mcp-config` flag.
  Each harness does this differently, and a *custom* harness may not speak MCP at
  all. The CLI is a PATH binary driven by **bash**, so it's **harness-agnostic
  with zero config** — it reuses the exact `engram-share`-style skill+symlink
  mechanism the engine already has.
- **Same interactive loop, plus video, proven on the dev-vm.** `open → snapshot`
  (accessibility tree with element refs) `→ click <ref> → observe → screenshot`,
  and `video-start / video-chapter / video-stop` (WebM). The CLI writes the
  snapshot to disk (refs stay local) rather than streaming the whole tree into
  context — Microsoft benchmarks ~4× fewer tokens than MCP.
- **It dodges the browser gotchas either way requires** but surfaces them once,
  in one config: `@playwright/cli`/`@playwright/mcp` default to the real `chrome`
  channel (absent in the guest) and Chromium's setuid sandbox fails as root —
  so we pin `chromium-headless-shell` + `--no-sandbox` (safe: the microVM *is*
  the isolation boundary) + bundle fonts/fontconfig (else text renders as boxes),
  all baked into the wrapper's config.

The CLI is the same Playwright actions the MCP server wraps — just with argv +
stdout instead of a protocol, which is what a shell-having coding agent wants.

## Consequences / risks

- **Snapshot embeds the drive path** → every FC-host image must carry the bundle
  at the canonical path. Capture **hard-fails** (clear `SandboxError`) if a
  declared bundle is absent, and restore asserts presence — so a snapshot only
  exists with all its bundles, and the operator-controlled enable surfaces a
  not-yet-rolled fleet loudly (roll the host image first, then enable).
- **Universal skill delivery now depends on the engine.** Mitigated by
  **agentd graceful-degrade at activation**: an absent/failed in-guest mount
  logs + skips the skill, never failing the session (distinct from the host-side
  presence hard-fail above).
- **glibc only.** The bundle is glibc-linked; **musl/alpine bases can't run it**
  (the mount succeeds, the binaries don't). The baker warns on `[browser]
  enabled` against a musl base.
- **Security.** Read-only drives, no new privileged guest capability, no new
  coord surface. The browser is a child of the harness at the session's
  privilege (consistent with ADR-0023's "skills run at session privilege") and
  reaches only guest localhost + whatever the egress proxy `allow_hosts` permits.

## Alternatives rejected

- **Bake Chromium into every image** — base-image-dependent, +500 MB dead weight,
  impossible via post-build injection.
- **An MCP server (`@playwright/mcp`)** — see "Why not an MCP server".
- **Host-side / remote browser over CDP** — can't reach the agent's
  guest-localhost dev server, defeating the primary use case.
- **A bespoke `engram-browser` CLI over a persistent Playwright daemon** —
  reinvents what `@playwright/cli` already maintains (the snapshot/click/video
  surface).

## Future: dynamic per-session skill/tool selection (deferred)

Today the *set* of RO mounts a session gets is **frozen at image-enable**: FC
can only attach drives *before* boot, never after `load_snapshot`, so a session
inherits exactly the device model its base snapshot was captured with. Adding a
mount to an image → re-enable it; updating a bundle's content → re-bake the
FC-host image (no re-enable). This matches ADR 0023's "fixed-at-boot" semantics.

If we later want **a session-start UX where the user picks skills/tools from a
library** (not in scope here):

- **Curated, heavy, shared (the browser).** Attach a *superset* at capture —
  e.g. one "library" squashfs holding the whole curated catalog — and have
  agentd activate only the user-selected subset per session (activation is
  already per-session). Library-wide selection, no re-enable, at the cost of
  every host staging the full library.
- **Light, dynamic, or user-uploaded (skills, small tools).** Don't ride a
  capture-time drive. agentd already *materializes* skills at `SpawnHarness`
  from a source dir — that source could instead be **fetched over a vsock seam**
  (the forge/upload-bridge pattern) from coord/BlobStorage into the writable
  rootfs at session start. Arbitrary per-session selection, no re-enable, no
  device-model change — the natural home for ADR 0023's still-deferred
  user-uploaded skills.

Likely end state: a **hybrid** — heavy curated tooling stays an RO host drive
(this ADR); light/dynamic/user-selected skills arrive via a session-start fetch
seam, with `engram-session-bundles::activate` as the shared delivery-agnostic
materialization layer.

## Implementation

The engine + central skills + the browser bundle shipped in **#55** (`AuxRoDrive`
+ capture wiring + FC attach/presence-re-anchor + init-shim mount + the
`engram-session-bundles` activation crate + retiring the bake-time injectors +
`[browser]` flag + ProcessBackend dev parity + the `deploy/bundles/*` recipes +
the CI bundle-publish + `dev_image` auto-rebake lanes). This revision (PR branch
`adr-0027-playwright-cli-migration`) swaps the browser consumer from
`@playwright/mcp` to `@playwright/cli` (bundle `build.sh`, the activation, the
`show-your-work` skill), and fixes two CI failures the #55 merge left on `main`
(the `publish-bundles` cleanup-permission bug + a `ha_listener` LISTEN/NOTIFY
test flake).

The **engrams-internal** deploy repo carries the FC-host packer step that stages
`/var/lib/engram/shared/*.squashfs`, the `engrams-bundles-changed` /
`engrams-dev-image-changed` dispatch handlers, and `[browser] enabled` on the
dev-engrams dogfood image.

Still pending before prod use: a full in-session dogfood (the `show-your-work`
loop against the engrams dev server, confirming guest-localhost reachability)
and the prod FC-host roll.

Out of scope (still deferred from ADR 0023): user-uploaded skills (registry +
upload API/UI + PG `skills` table + org enable/disable). This ADR delivers only
the platform-curated RO-bundle engine; user uploads ride it later.
