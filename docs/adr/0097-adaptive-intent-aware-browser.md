# ADR 0097 — Adaptive, intent-aware browser control

Status: Accepted

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

The production Playwright CLI is also several releases behind. A fair
comparison must therefore separate the client choice from the skill-policy
change.

## Decision

### One browser capability

Replace `show-your-work` with one `browser` skill. The evaluation described
below did not admit the hybrid candidate, so the production bundle upgrades
`playwright-cli` from 0.1.13 to 0.1.17 and keeps it as the sole browser
actuator. `agent-browser` 0.32.0 remains reproducible in the evaluation harness
but is not packaged. Playwright drives the same loopback-CDP Chromium that VNC
displays.

The normal loop is:

1. observe a current semantic snapshot;
2. search/read before choosing a target;
3. perform one action and wait for its expected condition;
4. take a fresh observation and verify the postcondition;
5. after ambiguity or one no-progress recovery, inspect an annotated screenshot;
6. never repeat an unchanged action against an unchanged page more than twice.

Annotated screenshots are internal observations. A browser screenshot saved
under `/tmp/engram-browser-observations` creates an interlock: no later browser
command runs until the harness-native `browser_view` tool returns that exact
image to the model. The pixels do not enter the Engram event stream.

The agent publishes a file only when the user explicitly requests evidence or
the requested deliverable is itself visual. A successful evidence request
produces one final screenshot by default. Video requires an explicit request or
a temporal bug.

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

The gate is a rollout selector, not a goal that may be weakened after seeing
results. Because the hybrid failed it, the bundle ships Playwright CLI 0.1.17
with the new browser and sharing policy while agent-browser remains confined to
the evaluation harness.

## Consequences

- Normal browser work uses current, searchable observations and explicit
  postcondition checks.
- Visual information is available for canvas, occlusion, unlabeled controls,
  and other cases where semantic page state is insufficient, without paying
  the screenshot cost for every action.
- Human VNC viewing and takeover are unchanged.
- The browser bundle does not grow an agent-browser executable. Each harness
  gains a small local image-observation tool enabled only for browser sessions.
- Live-model evaluation is on-demand because it consumes model quota and is
  nondeterministic. CI receives no model credentials and runs only the task,
  grader, command-bound contracts, and no-model smoke.
- agent-browser containment flags that are incompatible with an externally
  supplied CDP endpoint are not a security boundary. The existing microVM,
  loopback CDP binding, and session network policy remain the boundary.

## Implementation record

### Environment

The recorded local run used macOS 26.2 on arm64, Google Chrome
150.0.7871.115, Codex CLI 0.144.3, and Claude Code 2.1.211. Codex resolved to
`gpt-5.4`; Claude requested `sonnet` and resolved to `claude-sonnet-5`.

The browser arms were Playwright CLI 0.1.13, Playwright CLI 0.1.17, and
agent-browser 0.32.0. MiniWoB used BrowserGym MiniWoB 0.14.3, BrowserGym's
Python Playwright 1.44.0, and MiniWoB++ revision
`7fd85d71a4b60325c6585396ec4f48377d049838`. The production bundle pins Node
20.18.1, Playwright CLI 0.1.17, and Debian Chromium
149.0.7827.196-1~deb12u1.

### Incident Console matrix

The clean 64-episode run is recorded locally as
`artifacts/browser-eval/20260716T175757Z`. Each cell below pools four seeds,
plain/evidence prompts, and both harnesses (16 episodes per arm).

| Arm | Overall | Exact task | Share policy | Timeouts | Max identical repeat | Median commands on success | Codex / Claude overall |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Playwright 0.1.13 production | 8/16 | 12/16 | 14/16 | 8 | 4 | 19 | 5 / 3 |
| Playwright 0.1.17 + new policy | 11/16 | 16/16 | 11/16 | 0 | 1 | 17 | 7 / 4 |
| agent-browser DOM only | 0/16 | 0/16 | 16/16 | 0 | 1 | n/a | 0 / 0 |
| agent-browser + adaptive vision | 14/16 | 15/16 | 15/16 | 2 | 1 | 19 | 6 / 8 |

The generated Incident Console gate summary was:

- pass delta at least three: pass (+6 over production);
- Codex and Claude per-harness loss limits: pass;
- command protocol and no-more-than-two-repeat checks: pass;
- all episodes within bounds: **fail**; and
- exact sharing intent: **fail**.

The two hybrid failures were both Codex evidence episodes. Seed 0 completed the
exact task but timed out before publishing the requested final screenshot. Seed
3 timed out before selecting the canvas-derived service or submitting. This
produced two 181-second timeouts, one exact-task miss, and one evidence-policy
miss. Playwright 0.1.17 completed all 16 exact tasks without a timeout, but five
successful evidence episodes omitted the requested final share. The DOM-only
agent-browser arm never completed the canvas task.

### MiniWoB++ calibration

The first calibration attempt exposed an unbounded wait for a BrowserGym
driver response and was discarded. The harness now bounds task setup,
validation, and shutdown independently and records a structured infrastructure
failure if the driver exits or fails to reply. The clean 24-episode rerun is
recorded locally as
`artifacts/browser-eval/20260716T184455Z-miniwob` (six tasks, two arms, and both
harnesses at seed 0).

| Arm | Overall | Exact task | Timeouts | Repeat-limit failures | Command-bound failures | Median successful commands |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| agent-browser + adaptive vision | 0/12 | 0/12 | 10 | 4 | 1 | n/a |
| Playwright 0.1.17 + new policy | 0/12 | 0/12 | 12 | 0 | 4 | n/a |

Both arms failed every calibration episode, so the generated MiniWoB gate was
false and there was no successful-command median to compare. Three Playwright
episodes also recorded the BrowserGym driver exiting before its validation
reply; those remained explicit failures rather than being dropped. The
calibration therefore supplied no evidence that would override the primary
Incident Console gate or its predeclared Playwright fallback. Its uniformly
poor result is retained as a limitation of this local model/task calibration,
not presented as a product-performance claim.

### Selection and implementation

The hybrid is not shipped. This is the fallback specified before the run:
Playwright CLI 0.1.17 plus the unified intent-aware browser and sharing policy.
agent-browser stays in `evals/browser` so the comparison can be repeated when
its behavior or the model harness changes.

The implementation replaces `show-your-work`, narrows `share-file`, adds the
private `browser_view` bridge to both harnesses, recognizes both evaluated CLIs
when opening the browser pane, and extends the production-shaped Firecracker
VNC test from an RFB-banner check to semantic snapshot, highlighted screenshot,
and real framebuffer-pixel assertions.

The commit chain is:

1. `c2d5bb38` — `docs: propose adaptive browser ADR`;
2. `5d850f42` — `test: add browser evaluation harness`;
3. `4ec67a12` — `feat: add intent-aware browser capability`;
4. `81949844` — `test: strengthen browser and VNC coverage`; and
5. `docs: accept adaptive browser ADR with results` (this record).

Verification completed before acceptance:

- `just browser-eval-smoke`;
- the full 64-episode matrix and 24-episode MiniWoB++ calibration;
- browser bundle build and manifest inspection;
- targeted harness SDK, Codex harness, session-bundle, Process-backend
  activation, orchestrator, and web tests;
- web lint and production build; and
- `just check`.

The Firecracker/VNC case is `#[ignore]` locally because it requires Linux, KVM,
Firecracker, root, and the staged browser bundle. It remains wired into its
existing production-shaped CI lane.
