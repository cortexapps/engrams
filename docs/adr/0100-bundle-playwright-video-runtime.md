# ADR 0100: Bundle the Playwright video runtime

Status: 2026-07-16 — **Accepted.**

## Context

ADRs 0027 and 0097 make browser video an explicit, supported deliverable when
the user requests a recording or when a temporal bug requires one. The shared
headful-browser rewrite correctly stopped installing Playwright browser
binaries because the CLI now attaches to the already-running Chromium over
CDP. It also unintentionally removed Playwright's separately distributed
FFmpeg helper, which is required to encode video even when Playwright does not
launch the browser.

Production session `719b9424-23f3-4267-8052-69cac26817d2` exposed the gap:
`playwright-cli video-start` looked for
`~/.cache/ms-playwright/ffmpeg-1011/ffmpeg-linux`, and the attempted runtime
install failed because sandbox egress does not allow the Playwright artifact
CDN. `video-stop` could still print a WebM link even though no file existed.
The same missing-helper failure reproduces in a disposable container using the
current staged browser bundle.

## Decision

- At browser-bundle build time, use the exact `playwright-core` installed by
  the pinned `@playwright/cli` package to install only its target-native FFmpeg
  helper into the read-only bundle. Continue to omit Playwright browser
  binaries.
- Export `PLAYWRIGHT_BROWSERS_PATH` from the `playwright-cli` wrapper so the
  CLI daemon and every short-lived command resolve the bundled helper without
  a home-directory cache or runtime network access.
- Fail the bundle build if the Playwright-owned FFmpeg executable is absent.
- Extend the production-shaped browser/VNC Firecracker test to start and stop
  a recording and assert that a non-empty WebM exists before producing its
  private annotated screenshot.

## Consequences

The bundle grows by the small target-native FFmpeg artifact, but sessions no
longer need an egress exception or a mutable browser cache to record video.
The helper revision and architecture remain owned by the pinned Playwright
package, avoiding an independently versioned system FFmpeg dependency. Active
sessions mounted from an older bundle generation are unchanged; the repair is
available to sessions launched after the fixed bundle is published.

## Implementation and validation

The decision landed in `baf6d918` after the proposed ADR in `631ff0b6`.
Validation built a fresh arm64 browser bundle from the production build script;
the pinned Playwright package resolved FFmpeg revision 1011 and placed its
target-native executable under the bundle-owned browser path. The fresh bundle
then ran in a disposable Debian container with Docker networking disabled.
`video-start`, a chapter marker, and `video-stop` produced a non-empty 30,664
byte WebM without any writable Playwright cache or network access.

The bundle activation e2e passed, the ignored production-shaped
Firecracker/VNC test compiled, and that test now asserts a real WebM before its
existing semantic, annotated-screenshot, and framebuffer checks. The full
repository gate passed format, Clippy with warnings denied, Hakari, and all
1,747 nextest cases. The live Linux+KVM execution remains in the existing
Firecracker CI lane, where the browser bundle is built and mounted exactly as
production mounts it.

The full gate also found two independently merged ADR 0099 integration defects
already present on `main`: a duplicate workspace `proptest` dependency and a
property test that violated the concurrently added aligned-offset contract.
They are repaired separately in `fe30c900` and `cd22884d`; neither changes this
ADR's browser-runtime decision.
