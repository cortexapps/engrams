# RO session bundles (ADR 0027)

Read-only, content-addressed bundles the FC host mounts into guest sessions
as additional virtio-blk drives — the RO-mount skills/browser engine ADR 0023
deferred. agentd activates the right subset per session at `SpawnHarness`
(see the `engram-session-bundles` crate); the dev `ProcessBackend` symlinks
the unpacked trees and runs the same activation.

## Bundles

- **`skills/`** — always attached. The built-in skill wrappers
  (`engram-share`, `engram-pr`, `git-askpass`) + their `SKILL.md`. The
  unpacked tree *is* this directory. Mounted at `/opt/engram/skills`.
- **`playwright/`** — opt-in (`[browser] enabled` in an image's
  `engram.toml`). chromium-headless-shell + Node + Microsoft's
  `@playwright/cli` + all `.so` deps + a `playwright-cli` wrapper + the
  `show-your-work` skill, built by `build.sh` (glibc; **not** usable on
  musl/alpine bases). Mounted at `/opt/engram/browser`. The agent drives the
  browser via the `playwright-cli` CLI (bash) — no MCP server.
- **`sentinel/`** — ADR 0055: the tiny placeholder every reserved dynamic-mount
  slot (`dyn-0..dyn-{RESERVED_SLOTS-1}`) carries at base-snapshot capture, since
  Firecracker needs all drives present at `load_snapshot`. A per-session create
  `patch_drive`s the selected skill over a slot in the paused restore window;
  unused slots keep the sentinel and the guest skips it (`mount.json`
  `{"kind":"sentinel"}`).

## Building

```sh
# squashfs for the FC host image (CI / bake):
deploy/bundles/skills/build.sh      out/skills.squashfs
deploy/bundles/playwright/build.sh  out/playwright.squashfs    # needs Docker

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
