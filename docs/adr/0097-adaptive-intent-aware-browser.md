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
- Keep artifact creation/inspection separate from delivery in `share-file`.
- Prefix browser CLI calls with a concise, outcome-neutral
  `ENGRAM_BROWSER_INTENT`. The shared harness SDK recognizes supported browser
  invocations in native Shell/Bash calls and emits a typed `browser_activity`
  immediately before the generic tool start, carrying the same
  `tool_call_id`. It derives a safe fallback when the annotation is omitted.
- Use `browser_activity` to open the Browser pane once on first agent activity
  and to replace the raw shell card with a browser-specific presenter. Keep
  the correlated generic `tool_call_completed` event as the source of truth
  for success/failure, and never reopen after the user collapses the pane.

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

The browser/VNC production test must prove that the CLI observes semantic state,
that an annotated screenshot is created, and that the same page is painted in
the real framebuffer.

## Validation

The implementation passed the full Rust repository gate (format, clippy with
warnings denied, Hakari verification, and 1,690 nextest cases), targeted
harness enrichment, event-correlation, coordinator mapping, orchestrator, and
browser-pane tests, all 215 web tests, web lint and production build, bundle
activation coverage, and compilation of the ignored Firecracker browser/VNC
test. The latter remains assigned to the existing Linux+KVM CI lane, where the
browser bundle is staged before execution.
