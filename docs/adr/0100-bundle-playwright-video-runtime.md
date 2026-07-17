# ADR 0100: Bundle the Playwright video runtime

Status: 2026-07-16 — **Proposed.**

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

