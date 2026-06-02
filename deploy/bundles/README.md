# RO session bundles (ADR 0027)

Read-only, content-addressed bundles the FC host mounts into guest sessions
as additional virtio-blk drives — the RO-mount skills/MCP engine ADR 0023
deferred. agentd activates the right subset per session at `SpawnHarness`
(see the `engram-session-bundles` crate); the dev `ProcessBackend` symlinks
the unpacked trees and runs the same activation.

## Bundles

- **`skills/`** — always attached. The built-in skill wrappers
  (`engram-share`, `engram-pr`, `git-askpass`) + their `SKILL.md`. The
  unpacked tree *is* this directory. Mounted at `/opt/engram/skills`.
- **`playwright/`** — opt-in (`[browser] enabled` in an image's
  `engram.toml`). chromium-headless-shell + Node + `@playwright/mcp` + all
  `.so` deps + the `record-demo` skill, built by `build.sh` (glibc; **not**
  usable on musl/alpine bases). Mounted at `/opt/engram/browser`.

## Building

```sh
# squashfs for the FC host image (CI / bake):
deploy/bundles/skills/build.sh      out/skills.squashfs
deploy/bundles/playwright/build.sh  out/playwright.squashfs    # needs Docker

# unpacked trees for local dev (ProcessBackend):  use `just bundles`
```

## Distribution

CI builds + publishes each bundle as an OCI artifact to GHCR. The FC-host
image bake (engrams-internal) pulls them and stages each at a fleet-canonical
**stable symlink** — `/var/lib/engram/shared/<name>.squashfs` →
`<name>-<sha>.squashfs`. The base snapshot embeds the stable symlink path, so
restore re-anchors by presence and a version roll (repoint the symlink) needs
no session-image re-bake. See ADR 0027.
