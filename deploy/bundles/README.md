# RO session bundles (ADR 0027)

Read-only, content-addressed bundles the FC host mounts into guest sessions
as additional virtio-blk drives — the RO-mount skills/browser engine ADR 0023
deferred. ADR 0055: skills are **profile-selected per session**, not attached
per-image — a session creates with `selected_skills` (resolved from its
profile), the coord `patch_drive`s each into a reserved dynamic slot, and the
guest mounts it at `/opt/engram/dyn/<i>`. agentd's `activate()` reads each
bundle's in-squashfs `mount.json` and wires the declared skills (see the
`engram-session-bundles` crate); the dev `ProcessBackend` symlinks the unpacked
trees at the same `dyn/<i>` paths and runs the same activation.

## Bundles

Each skill bundle ships a `mount.json` (the activation contract: `kind`, the
skills it carries + their PATH wrapper bins + any `requires_env` gate, an
optional `provides_askpass`). Wrappers are **position-independent** — they
self-locate their runtime from `$0`, never a fixed mount path.

- **`skills/`** — the built-in skill wrappers (`engram-share`, `git-askpass`)
  + their `SKILL.md`. The unpacked tree *is* this directory.
  Selected by name `skills`.
- **`browser/`** (ADR 0065) — the shared live browser: chromium (full UI) + Xvfb
  + x11vnc + openbox + Node + Microsoft's `@playwright/cli` (pointed at that
  Chrome over CDP — **no** headless-shell) + a `playwright-cli` wrapper + the
  `show-your-work` skill + all `.so` deps, built by `build.sh` (glibc; **not**
  usable on musl/alpine bases — size the selecting profile's image for a
  browser). The human drives it over VNC (BROWSER tab) and the agent drives the
  SAME Chrome via the `playwright-cli` CLI (bash, no MCP server) — one shared
  browser. Selected by name `browser`. (Subsumes the retired headless-only
  `playwright` bundle.)
- **`ide/`** (ADR 0085) — the opt-in in-guest IDE: the pinned code-server
  standalone release (VS Code web) + the `engram-ide` launcher agentd drives
  lazily (StartIde → `engram-ide --ensure`). The bundled Node is glibc-dynamic,
  so build.sh gives it the same patchelf self-contained treatment as `browser/`
  (**not** usable on musl/alpine bases). Binds loopback `:13337` only — reached
  over the ADR 0066 vsock relay through the session-scoped orchestrator proxy.
  A trusted first-party surface like the shell: full session env, no uid drop
  (unlike `browser/`). Selected by name `ide`.
- **`sentinel/`** — ADR 0055: the tiny placeholder every reserved dynamic-mount
  slot (`dyn-0..dyn-{RESERVED_SLOTS-1}`) carries at base-snapshot capture, since
  Firecracker needs all drives present at `load_snapshot`. A per-session create
  `patch_drive`s the selected skill over a slot in the paused restore window;
  unused slots keep the sentinel and the guest skips it (`mount.json`
  `{"kind":"sentinel"}`).

## Building

```sh
# squashfs for the FC host image (CI / bake):
deploy/bundles/sentinel/build.sh    out/sentinel.squashfs      # MANDATORY (capture)
deploy/bundles/skills/build.sh      out/skills.squashfs
deploy/bundles/browser/build.sh     out/browser.squashfs       # needs Docker
deploy/bundles/ide/build.sh         out/ide.squashfs           # needs Docker

# content-addressed staging + current.json stamp for a dev FC host:  `just bundles-squashfs`
# unpacked trees for local dev (ProcessBackend):  use `just bundles`
```

## Distribution

CI builds + publishes each bundle as an OCI artifact to GHCR. The FC-host
image bake (engrams-internal) pulls them and stages each **content-addressed**
— `/var/lib/engram/shared/<sha256>.squashfs` (ADR 0055: content-keyed, no name
prefix, so a skill staged once dedups across whatever slot it lands in) — plus
a `current.json` stamp (logical name → sha). There is deliberately no fixed/mutable path: base
snapshots pin the exact generation they captured against, missing generations
materialize from BlobStorage (`bundles/sha256/<sha>`), and fresh session
creates swap to the host's current generation while load-paused — so a skill
edit still ships fleet-wide on the next host roll with no session-image
re-bake. Retention is the GC pin set (referenced-by-any-snapshot), not a
keep-N policy. See ADR 0035 (which supersedes ADR 0027's stable-symlink
distribution after the 2026-06-03 bundle-skew incident).
