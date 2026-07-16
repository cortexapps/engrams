# ADR 0097 — Adaptive, intent-aware browser control

Status: Proposed

## Context

The opt-in `browser` bundle gives the agent and human one shared headful
Chromium: the agent drives it over CDP and the human watches or takes over over
VNC (ADR 0065). The first agent surface was `playwright-cli` 0.1.13 plus a
`show-your-work` skill. Two shortcomings have become clear:

1. an agent can repeatedly act on stale page state without noticing that it is
   making no progress; and
2. `show-your-work` conflates observing a page with publishing evidence, so an
   internal screenshot can become an unsolicited conversation artifact.

Agent-oriented browser clients provide a useful missing layer: compact
interactive snapshots, stable element references, annotated screenshots whose
labels match those references, and bounded command execution. The important
change is not CDP versus Playwright. It is a closed observation loop with a
deliberate visual fallback and a separate decision about whether evidence is
part of the user's requested deliverable.

The production Playwright CLI is also several releases behind. Versions after
0.1.13 fixed unbounded waits and screenshot/CDP races, so a fair comparison must
separate the client choice from the skill-policy change.

## Decision

### One browser capability

Replace `show-your-work` with one `browser` skill. `agent-browser` 0.32.0 is the
default browser actuator, attached to the existing loopback CDP endpoint. The
bundle retains `playwright-cli`, upgraded to 0.1.17, for Playwright-specific
testing, tracing, mocking, and cross-browser work. Both drive the same Chromium
that VNC displays.

The normal loop is:

1. observe a compact interactive snapshot;
2. search/read before choosing a target;
3. perform one action and wait for its expected condition;
4. take a fresh observation and verify the postcondition;
5. after ambiguity or one no-progress recovery, inspect an annotated screenshot;
6. never repeat an unchanged action against an unchanged page more than twice.

Annotated screenshots are internal observations. The agent publishes a file
only when the user explicitly requests evidence or the requested deliverable is
itself visual. A successful evidence request produces one final screenshot by
default. Video requires an explicit request or a temporal bug.

The generic `share-file` skill is correspondingly narrowed: it delivers an
already-created artifact on request; it does not decide that work ought to be
shown.

### Evaluation before rollout

`evals/browser` contains a deterministic BrowserGym-compatible task contract
and a black-box runner for Codex and Claude. The primary Incident Console task
combines ordinary DOM interaction, a DOM rerender, an obstructing overlay, and
one canvas-only topology decision. Four deterministic seeds run with and
without a final-evidence request. The server grades exact application state and
the runner separately records browser commands, no-progress loops, screenshots,
timeouts, and `engram-share` calls.

The comparison arms are:

1. Playwright CLI 0.1.13 with the original skill;
2. Playwright CLI 0.1.17 with the new policy;
3. agent-browser 0.32.0 without visual fallback; and
4. agent-browser 0.32.0 with adaptive annotated screenshots.

Every episode is bounded to 180 seconds and 30 browser commands. The hybrid is
accepted only if, across Codex and Claude, it beats the production baseline by
at least three of sixteen pooled task variants, neither harness loses more than
one variant, all runs terminate within their bounds, no unchanged action is
repeated more than twice, no-evidence prompts produce zero shares, and every
successful evidence prompt produces exactly one final share. The top two arms
are also calibrated on six MiniWoB++ task shapes before acceptance.

If agent-browser does not pass the gate, the bundle ships Playwright CLI 0.1.17
with the new browser and sharing policy while agent-browser remains confined to
the evaluation harness.

## Consequences

- Normal browser work uses smaller, searchable observations and explicit
  postcondition checks.
- Visual information is available for canvas, occlusion, unlabeled controls,
  and other cases where semantic page state is insufficient, without paying
  the screenshot cost for every action.
- Human VNC viewing and takeover are unchanged.
- The browser bundle grows by one architecture-selected native executable, not
  the multi-platform npm package.
- Live-model evaluation is on-demand because it consumes model quota. The task,
  grader, command bounds, and no-model smoke run in CI.
- agent-browser containment flags that are incompatible with an externally
  supplied CDP endpoint are not a security boundary. The existing microVM,
  loopback CDP binding, and session network policy remain the boundary.

## Implementation record

To be completed before changing the status to `Accepted`: exact CLI/model
versions, the 64-episode matrix, MiniWoB++ calibration, failures, selected arm,
and verification commands.
