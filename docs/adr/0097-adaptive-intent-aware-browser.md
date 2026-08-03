# ADR 0097: Adaptive, intent-aware shared browser

Status: Accepted

## Context

The previous `show-your-work` skill treated browser use primarily as a way to
produce screenshots and recordings. That made ordinary navigation prone to
unrequested artifacts, while the text-only browser interface could not use the
pixels already rendered by the shared headful Chromium when semantic state was
insufficient.

We compared Playwright CLI 0.1.13, Playwright CLI 0.1.17, and agent-browser
0.32.0 with Codex and Claude across semantic and canvas-dependent variants of
a deterministic incident-console task. CLI choice, skill policy, visual
capability, harness, and evidence intent were varied independently. Pass/fail
used exact task state and artifact counts rather than an LLM judge. The full
experimental implementation and raw iteration history are preserved on the
`codex/browser-eval-lab` branch rather than shipped in the product.

## Decision

- Replace `show-your-work` with one `browser` skill for ordinary browser work,
  testing, and visual recovery.
- Ship Playwright CLI 0.1.17 attached over loopback CDP to the existing shared
  headful Chromium. Keep its testing, tracing, mocking, PDF, and cross-browser
  surface available.
- Expose a harness-native `browser_view` tool. It accepts only bounded image
  files under approved guest paths and never emits a user-visible artifact.
- Start from semantic browser state. Escalate to one private screenshot only
  for a missing visual fact or unresolved ambiguity, then return to semantic
  verification.
- Enforce the screenshot-to-`browser_view` handoff in the browser wrapper so
  agents cannot accidentally act on screenshot command output as if it
  contained pixels.
- Share nothing during ordinary browser work. Share one final screenshot only
  for explicit evidence or an inherently visual deliverable. Use video only
  for explicit requests or temporal bugs.
- Bundle the target-native FFmpeg helper owned by the pinned Playwright package
  and point the CLI daemon at that read-only bundle path. Video must not depend
  on a per-user browser cache, a runtime download, or an artifact-CDN egress
  exception; Playwright browser binaries remain omitted because the CLI
  attaches to the shared Chromium over CDP.
- Keep artifact creation/inspection separate from delivery in `share-file`.
- Prefix browser CLI calls with a concise, outcome-neutral
  `ENGRAM_BROWSER_INTENT`. The shared harness SDK recognizes supported browser
  invocations in native Shell/Bash calls and emits a typed `browser_activity`
  immediately before the generic tool start, carrying the same
  `tool_call_id`. It derives a safe fallback when the annotation is omitted.
- Use `browser_activity` to open the Browser pane once on first agent activity
  and to replace the raw shell card with a browser-specific presenter. Only
  LIVE activity opens the pane — an event stamped after the human opened the
  session. The SSE feed replays the whole durable log, so a session the agent
  browsed earlier must not pop the browser open on every later visit. Keep the
  correlated generic `tool_call_completed` event as the source of truth for
  success/failure, and never reopen after the user collapses the pane.

## Evaluation findings

- Exact task-state success saturated at 64/64 across the controlled matrix, so
  the incident task was useful as a contract test but not evidence of a
  reliability winner.
- With the concise policy, both candidate CLIs completed 16/16 matched
  episodes. Playwright used a median 12 commands in 62.7 seconds; agent-browser
  used 14 commands in 51.4 seconds. Agent-browser was faster but exceeded the
  shipping gate of within 10% of the best median successful command count.
- A prescriptive observe/act/wait/observe loop did not improve task success and
  increased commands and latency. One agent-browser/Claude episode also took
  an unnecessary second screenshot after semantic state already confirmed
  success.
- Automatically appending compact snapshots after agent-browser mutations was
  rejected after a targeted trial: it refreshed references at surprising
  points and omitted non-interactive success text, causing both visual trials
  to fail. Verification must be selected for the postcondition, not applied as
  a blanket snapshot after every action.
- Playwright 0.1.13 and 0.1.17 both passed 16/16 under identical instructions;
  the upgrade is therefore maintenance and capability consolidation, not a
  claimed reliability gain.

## Consequences

The product gains visual recovery without coupling internal observations to
sharing. Playwright remains the default because it met the complete gate and
bundles useful post-action state into fewer agent round trips. Agent-browser is
not shipped by this decision; the preserved lab branch can support future
comparison on broader standardized tasks without expanding this PR.

Browser activity follows ADR 0089's correlation and presentation principles
without entering its registered-tool dispatch protocol: this is enrichment of
an already-running native shell call, not a new model-facing tool awaiting a
result. Detecting framebuffer changes was rejected as the trigger because it
would miss read-only inspection, fire for animations or human input, and
require mounting the VNC viewer before the pane knows it should open.

The video-runtime addition follows a production failure in session
`719b9424-23f3-4267-8052-69cac26817d2`: `video-start` looked for
`~/.cache/ms-playwright/ffmpeg-1011/ffmpeg-linux`, while the shared-browser
bundle contained Chromium and Playwright CLI but not Playwright's separately
distributed encoder. A runtime install could not reach the artifact CDN, and
`video-stop` could print a WebM link even though no file existed. Installing
only the version-matched helper during the bundle build preserves the shared
CDP design without broadening sandbox egress. Sessions mounted from an older
read-only bundle generation are unchanged; the repair applies after the fixed
bundle is published.

The browser/VNC production test must prove that the CLI records a non-empty
WebM, observes semantic state, creates an annotated screenshot, and exposes the
same painted page through the real framebuffer.

## Validation

The implementation passed the full Rust repository gate (format, clippy with
warnings denied, Hakari verification, and 1,690 nextest cases), targeted
harness enrichment, event-correlation, coordinator mapping, orchestrator, and
browser-pane tests, all 215 web tests, web lint and production build, bundle
activation coverage, and compilation of the ignored Firecracker browser/VNC
test. The latter remains assigned to the existing Linux+KVM CI lane, where the
browser bundle is staged before execution.

The video-runtime follow-up built a fresh arm64 bundle with Playwright FFmpeg
revision 1011, then ran it in a disposable Debian container with Docker
networking disabled. `video-start`, a chapter marker, and `video-stop` produced
a non-empty 30,664-byte WebM without a writable Playwright cache or network
access. The bundle activation e2e and Firecracker/VNC test compilation passed;
the latter now records and asserts a real WebM before its existing semantic,
annotated-screenshot, and framebuffer checks. The contemporary full repository
gate passed all 1,747 nextest cases in addition to format, Clippy, and Hakari.
