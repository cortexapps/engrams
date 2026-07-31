# ADR 0094: First-prompt delivery latency — wake the deliver op when the boot completes

- Status: **Proposed** — root-caused + fixed 2026-07-15; local-stack (VZ)
  reproduced + validated. See "Correction" — the first cut (PR #676) wired
  the wake onto the wrong boot path and did not move TTFM.
- Date: 2026-07-15
- Supersedes the abandoned "resume storm-shield" draft that first held
  this number (PR #675, closed unmerged — see Rejected alternatives).
- Related: ADR 0079 (the session-op executor + the *resume* sibling-
  deliver wake this extends), ADR 0092 (whose "40 s = guest stampede"
  TTFM reading this corrects; its density work is unaffected),
  engrams#669 (nice-boost, reverted — same misdiagnosis).

## Symptom

Time-to-first-message (session create → first assistant token) on the
dev-brain image was ~40 s. ADR 0092's WS0 campaign attributed it to the
resumed guest's JVM wake-up stampede starving the freshly-spawned
harness.

## Root cause (measured on the dev VM, real prod dev-brain base)

The ~40 s is **the coordinator deferring the initial prompt's `Deliver`
op through the session's boot, then waiting out its retry backoff** —
not the guest, not claude, not our harness adapter, not egress:

- `claude --version`: **225 ms** (binary + node runtime load is cheap).
- bare `claude --print "pong"` (real API): **3.0 s**.
- claude with the *exact* harness flags, prompt fed immediately: **2.3 s**.
- Full harness path: **40 s** — the adapter spawns claude, then dead
  silence until the turn completes; claude answers ~2 s after it finally
  receives the prompt on stdin.

The coordinator enqueues a `Deliver` op for the create prompt while the
session is still `pending` (booting). Deliver can't run yet → it defers
("session is pending — not deliverable") and is requeued on a **linear
backoff** (`(attempts+1)×2 s`, capped 60 — `session_ops.rs`). Nothing
makes it *ready* again when `CreateBoot` flips the row to `Active`, so
the accumulated backoff (4+6+8+10…s) plus the 5 s fallback poll elapses
before the prompt is forwarded — ~36 s after the session is actually
serviceable. The `/proc` trace of the waiting claude confirms it: idle
in `epoll_pwait`, no CPU, no reads — waiting for stdin that hasn't
arrived.

This is the **same class ADR 0079 already fixed for resumes**: a
`Resume{flavor=for_delivery}` (prompt-after-idle) left its sibling
Deliver backed off, and ADR 0079 wakes it on the resume's success. The
*fresh-create* boot was never given the same wake.

## Correction (2026-07-15, local VZ stack) — the wake was on the wrong path

PR #676's first cut extended the wake to the **`CreateBoot` op** (the
queue-scanner's `enqueue_boot_op`). Measuring it on the local stack
showed **no movement** — because a capacity-available create *never runs
a `CreateBoot` op*. `reserve_placement` binds a host at request time and
the create boots **out-of-op**, straight through
`session_boot::boot_on_reserved_host` (`SessionFence::unfenced()`); only
a create that finds *no* capacity is parked on the queue and later booted
via the `CreateBoot` op. So #676 wired the wake onto the rare (no-host)
path and left the common one untouched — the deliver still waited out its
backoff on every normal create.

Ground truth from `session_ops` for a fresh create: **no `CreateBoot`
row exists**, only the create-time `deliver` rows, whose `not_before`
stayed pinned at the backoff value (never pulled to boot-done). Exactly
the "#676 didn't help" signature.

## Decision

Wake the sibling Deliver at the **Active-transition tail of
`boot_on_reserved_host`** — the single chokepoint **both** boot paths
funnel through (the direct capacity-available create *and* the
`CreateBoot` op's verb both call it). `start_agent` has already attached
the harness by that point, so the prompt is deliverable the instant we
flip `Active`; `op_wake_queued_kind(session_id, Deliver)` pulls the
backed-off deliver's `not_before` to now + NOTIFYs, and the executor
forwards in <100 ms. Idempotent (0 rows matched when there is no
create-time prompt, or when the completion re-drive already claimed it);
the 5 s fallback poll still backstops a missed NOTIFY.

The `CreateBoot`-op wake from #676 is **kept** — it is not redundant: it
covers (a) a *terminal boot failure* (the tail wake never runs — the fn
returns `Err` — so the op-level Done-or-terminal wake keeps the
`gone`/failed path fast) and (b) `Resume{flavor=for_delivery}`. The two
wakes overlap only on the `CreateBoot` *success* path, where the second
is a harmless no-op. No new op, no wire change, no knob; delivery
durability is unchanged (the Deliver op is still the durable owner).

## Validation (local VZ stack, prod-shape coordinator path)

Demo image, claude harness, create-with-prompt. `prompt_received →
run_started` **before**: ~5.0 s (deliver op `not_before` pinned at its
`(attempts+1)×2 s` backoff, then a ~1 s fallback-poll claim). **After**:
the deliver op's `not_before` is pulled to boot-`Active` and it *finishes
forwarding within ~15 ms of Active* — the coordinator-side cost is gone.
End-to-end `prompt_received → run_started` lands at ~2.7 s.

The prod impact is larger than the local delta: local boot is ~0.5 s so
the deliver defers only once (attempts=1, 4 s); on prod dev-brain the
slow base-restore makes it defer repeatedly, so the *accumulated* linear
backoff is what reached ~36 s. Removing it collapses that whole staircase
to boot + <100 ms.

## Open items (measured 2026-07-15, not yet fixed here)

1. **Residual ~2.2 s `Active → run_started`, first-turn only.** With the
   coordinator fixed, the deliver forwards to the host `cmd_tx` at
   boot+~15 ms and the command reaches the **in-guest engine `cmd_rx` in
   ~24 ms** (guest-instrumented) — the wire is fast. Yet `run_started` is
   recorded a **rock-steady ~2.2 s** later. Bisected with logs on both
   ends (measured on the local VZ stack, with a real API-backed turn):
   - the host→coord event sink is fast (every event appends in ≤22 ms);
   - the coord `reader_loop` sits `read_msg`-blocked waiting for
     `run_started` from the guest for the whole ~2.2 s;
   - so the hold is **guest-side**, between the engine dequeuing the first
     Prompt and `run_started` reaching the wire — inside the harness
     `start_turn`→`emit`→`pump_events` path (`emit` is *not* channel
     backpressure: the event channel is 1024-deep). It is **first-turn
     only**: a follow-up prompt to the already-warm harness reaches
     `run_started` in **~30 ms**.
   This code is **shared across backends** (harness SDK + adapter, not the
   VZ vsock bridge), so it may affect prod too — must be re-measured on the
   dev VM / Firecracker. The exact sub-cause (which `pump_events` write
   stalls, and why only the first) needs one more guest log line; local
   iteration is currently gated by the all-bundles `just bundles-vz`
   rebuild (~20 min per harness change). Tracked separately; not addressed
   here.
2. **`run_started → first response` (the ~33 s the users actually feel)**
   is a *turn-execution* number, downstream of everything here. Locally
   (demo image, real API key injected) a full claude turn is **~2.3–2.8 s
   — matching bare claude, so the harness wrapper does not inflate it**;
   the ~33 s does **not** reproduce on the demo image and is dev-brain-
   specific (heavy MCP/context/tools). Reproducing it needs the real
   dev-brain base on the dev VM (Firecracker) — the prod image is
   amd64-only, so it can't run on the arm64 VZ backend, and building
   dev-brain locally for arm64 is impractical (four private repos + a
   long Gradle/pnpm prewarm). Untouched by this ADR.
3. **"Broken steering / queued messages" is queue-by-design, not a
   delivery bug.** The claude adapter QUEUES a `Prompt` that arrives
   mid-turn (`pending.push_back`, emits `PromptQueued`) and only writes it
   to claude when the current turn's `result` lands, then runs it
   back-to-back; the only in-flight *redirect* is an explicit
   `HarnessCommand::Interrupt` (`control_request` → abort → then the
   queued prompt runs). A second stdin `user` line does not interrupt.
   Whether the product wants true steering (interrupt-then-inject) is a
   separate decision; the mechanism is not lossy.
   *Update 2026-07-31: the product decided — a mid-turn `Prompt` now
   auto-fires the `control_request` interrupt and the queued prompt runs
   back-to-back on the same warm process (prod incidents showed type-ahead
   waiting 7–56 min for hour-long turns to end). See ADR 0052 decision 5.*

## Rejected alternatives (how we found it)

- **Resume storm-shield** (this ADR's original draft; PR #675): agentd
  SIGSTOPs the resumed workload so the harness cold-starts on a quiet
  guest. Built and measured — under a **60 s full-workload freeze the
  TTFM was still ~36 s**, which is what disproved the stampede theory and
  sent the investigation to the prompt-delivery path. Abandoned.
- **Harness nice-boost** (engrams#669, reverted): −10 niced the harness
  to out-run the stampede. Also aimed at the misdiagnosed cause; measured
  no effect. Reverted for one-path clarity.
- **A shorter Deliver backoff**: would shave a few seconds but still
  leaves the prompt waiting on a poll tick after boot; the event-driven
  wake is the correct fix (and is what ADR 0079 already established).

## Consequences

- The **coordinator-side** deliver backoff — the fresh-create staircase
  that reached ~36 s on prod dev-brain — is removed; the prompt is
  forwarded within ~100 ms of `Active`. This is the prod-relevant win.
- TTFM is **not** fully closed by this change: two measured gaps remain
  downstream (the VZ ~2.2 s command-transport residual, and the
  `run_started → response` turn latency) — see Open items. Do not read
  this ADR as "TTFM solved."
- The harness adapter's *invocation* is exonerated as a fixed-overhead
  source (the persistent stream-json path is ~4–6 s to first token on
  fast HW, same order as bare claude); no shield, no nice-boost.
- ADR 0092's density work (File-unpinned base, lazy base-shm, orphan
  reap) is untouched and remains correct; only its TTFM attribution is
  superseded here.
