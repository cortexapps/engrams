# ADR 0097: Adaptive, intent-aware shared browser

Status: Proposed

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

The browser/VNC production test must prove that the CLI observes semantic state,
that an annotated screenshot is created, and that the same page is painted in
the real framebuffer.
