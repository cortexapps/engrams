# 0108 — First-token delivery hardening

Status: Proposed (2026-07-31)

## Context

Two production sessions on 2026-07-31 (6f3dac3e, 454c5e21) showed a 50-second
gap between the create prompt and `run_started`. The investigation found five
stacked defects. Each defect was a reasonable fix for an earlier problem.

1. ADR 0073 moved the create prompt onto the durable outbox. This was correct:
   the old `ENGRAM_INITIAL_PROMPT` env rail had no in-guest consumer. The cost:
   first delivery now requires the coordinator to observe the harness attach.
2. ADR 0094 added a wake that runs the Deliver op when `CreateBoot` completes.
   That milestone is the agentd handshake. The harness attaches its own vsock
   connection 50–200 ms later. The Deliver op therefore races the attach on
   almost every fresh create, and loses: `send_prompt` returns `NotFound`.
3. The `NotFound` remedy (post-#594) treats the error as "harness gone" and
   fires `start_agent` at once. agentd delivers SIGUSR1 to the harness. When
   the race in (2) fired the remedy, the SIGUSR1 hit a harness that was mid-
   attach and dropped a healthy connection.
4. The redial then hit a vsock black hole. A checkpoint queues a vsock
   `TRANSPORT_RESET` (see `vsock_epoch`). The guest→host `HarnessAttach` frame
   can be swallowed in the RX-gate window (PR #596 / `fa8a625d` fixed the
   plain-resume arm; the snapshot-create arm stays open). vsock does not
   retransmit. Both sides then park forever: the SDK on its ack read
   (`engram-harness-sdk/src/lib.rs`, no timeout, no log) and the host on its
   attach read (`engram-host-agent/src/harness.rs`, no timeout, no log). The
   SDK dial also sits outside the SIGUSR1 `select!`, so a hung dial is
   nudge-proof.
5. The only escape is the next coordinator reattach (SIGUSR1). Its cadence is
   the op-retry backoff, which the pre-Active "session is pending" deferrals
   had already inflated. Recovery took 41 seconds and was set by a backoff
   counter, not by the data path.

A fourth stall shape surfaced in production after A1–A5 shipped (session
7eddce62): a prompt to a session parked for 27 minutes un-parked the VM in
120 ms, but the harness vsock link had died during the pause while the host
hub still advertised the handle. `send_prompt` returned Ok into a socket
with no reader, so the outbox row was marked delivered and waited out the
full 30 s ACK_TIMEOUT before the retry hit `NotFound` and re-established
the harness in 70 ms. A1–A5 all key on signals this shape never produces:
there is no failed attempt to back off from, no boot milestone, and the
attach signal alone only wakes an op that finds the row still gated by
`not_before`.

Two latent holes surfaced during the same investigation:

- The issue-#218 generation guard protects only connection *removal*. A stale
  `drive_attached` task can still insert after a newer one and clobber it.
- The host acks an attach (`ok: true`) before it registers the connection. A
  failure between the two steps leaves an ack'd, unregistered harness.

A related stream-contract defect: the orchestrator SessionListener uses
"frame has no idx" as its lag test. Ephemeral `agent_message_chunk` frames
also have no idx. The lagged path reconnects with no backoff. Result: ~5
reconnects per second for the full duration of every generation on every task
session.

## Decision

We restate the goal as three invariants and build to them. Time to first
token (TTFT) must equal boot time plus model latency. Nothing else.

**Invariant 1 — bounded attach.** A harness is attached, or some component
finds out, within bounded time. No unbounded, unlogged wait exists anywhere
on the attach path.

**Invariant 2 — ack implies registered.** When the SDK reads `ok: true`, the
host hub has the connection registered. Registration is ordered by
generation: an older connection can never clobber a newer one.

**Invariant 3 — delivery keys on attach, not on boot.** The Deliver op wakes
on evidence of the attach (the first harness event), not on `CreateBoot`
completion. The destructive reattach fires only for a harness that was
established and then lost, never inside an attach grace window.

### Workstream A — delivery path

- **A1** SDK: put a timeout on the attach write + ack read; return
  `Dropped` and redial. Put the dial under the SIGUSR1 `select!` and bound
  it. Host: put a timeout on both handshake reads.
- **A2** Host: register the connection before the ack write. Make the insert
  generation-ordered (insert only if absent or older). On ack-write failure,
  run the guarded remove.
- **A3** Coordinator: wake due Deliver ops when the first harness event for a
  session arrives. Keep the `CreateBoot` wake as a fallback.
- **A4** Coordinator: make the `NotFound` remedy age-aware. Inside an attach
  grace window (binding younger than N seconds, no prior harness event this
  epoch), defer without a reattach. Fire the destructive reattach only after
  the grace, or when a previously-attached harness goes silent.
- **A5** Coordinator: pre-Active deferrals must not inflate the post-Active
  retry cadence. Reset the op attempt counter on the first attempt after the
  session turns Active.
- **A6** (endgame) The initial prompt rides `AgentSpec` / `start_agent` with a
  real in-guest consumer. The outbox row stays the durable at-least-once
  record. `prompt_id` dedup makes double delivery safe. TTFT becomes boot
  time with zero delivery round-trips. ADR 0073's failure was the missing
  consumer, not the design.
- **A7** The harness plane subscribes to the `vsock_epoch` severance watch and
  tears down hub connections on a bump. Add a ping/pong liveness frame so a
  black-holed established connection is detected without a coordinator
  SIGUSR1. Continue the RX-gate hardening in the vendored Firecracker for the
  snapshot-create arm.
- **A8** Coordinator: fresh attach evidence recalls waiting outbox rows.
  A new store primitive, `outbox_make_due`, is the inverse of
  `outbox_defer`: it pulls every un-acked row with a future `not_before`
  back to due. It never bumps `attempts` (a recall is not a delivery try)
  and it is idempotent (`not_before > now` only). Two call sites: the A3
  attach signal calls it before the op wake, so the wake finds the row
  due; and the heartbeat disagreement check becomes a repair —
  `harness_desync::run_once` recalls the rows and enqueues the Deliver op
  directly for each Active session whose sandbox is running but not
  hub-attached. `run_once` is a pure step over two sandbox sets, so the
  DST swarm drives it without the HTTP handler. This promotes the
  disagreement signal from the alarm-only metric this ADR previously left
  as follow-up (Workstream E's cosim oracle) to a production repair.

### Workstream B — stream contract

- Add `durable_only` to `StreamEventsRequest`. The coordinator suppresses
  ephemeral frames for subscribers that set it. The SessionListener sets it.
- The listener keys its lag test on `kind == "lagged"` and skips any other
  idx-less frame. This alone stops the storm under deploy skew.
- Add a frame-taxonomy contract test on the orchestrator side: enumerate
  every frame kind the coordinator can emit; an unknown kind fails the test.

### Workstream E — DST ratchet

- Split the sim harness into its own FSM: `Dialing → Attached →
  Dropped(redialing)`, with fault points (attach delayed, frame swallowed,
  redial hangs, ack lost).
- Add a TTFT liveness oracle: a session that is Active with a due outbox row
  emits `run_started` within N sim-seconds.
- Promote the attach disagreement metric to a cosim oracle: coordinator-
  Active + host-running + hub-unattached must not persist past N sim-seconds.
- Add a hub oracle to the host swarm: at quiescence, SDK-believes-attached
  equals hub-has-registration.
- Pin regression seeds for the incident interleaving.
- A6 lands only with an exactly-once oracle: `run_started{prompt_id}` fires
  exactly once per prompt across spawn, attach, redelivery, and respawn
  interleavings (conformance rule, ADR 0098 D4).

Interrupt attribution and the web Esc fix ship as ordinary fixes (no ADR per
the 2026-07-31 policy): disable the library `cancelOnEscape`, gate the
interrupt on client run state, and thread an interrupt `source` tag onto the
`run_interrupted` payload.

## Consequences

- The attach path gains timeouts and logs. The 41-second silent zombie
  becomes a ~5-second logged self-heal, independent of coordinator retries.
- The boot-time reattach race disappears: delivery waits for the attach
  signal instead of probing and firing a destructive remedy.
- One wire-compatible proto field (`durable_only`). Old coordinators ignore
  it; the hardened listener no longer storms either way.
- A6 is a behavior change to `start_agent` (carries the initial prompt) and
  needs a harness re-bake. Double delivery is safe by dedup.

## Findings from the swarm work (2026-07-31)

Building the attach plane and oracles surfaced three coordinator defects:

1. **Fixed in this PR (measured mechanism):** a claim-loop livelock in
   `drive_session`. When every op attempt burns more virtual time than
   its requeue delay (a down host: 3600 s evict burns, 120 s deliver
   deadline burns, against a ≤60 s-capped backoff), each requeue leaves
   the sibling already due and the loop never runs dry — the scheduler
   stuck inside one step for 2.5 virtual years (640k claims on one op,
   seed 33043259). Duplicate budget-less Deliver ops were the unbounded
   fuel (the shim's pending-op guard is advisory). Fix: both retry arms
   requeue with `attempt_elapsed + delay` (strictly additive pacing — a
   claim cycle provably terminates), and the Deliver verb sweeps queued
   duplicate Deliver ops at claim (outbox rows stay the durable state,
   so the sweep is never lossy). An earlier `max(delay, elapsed)` clamp
   on the `RetryAfter` arm alone was a measured no-op: the churn goes
   through the `Retry` arm, and `max()` recreates the already-due
   boundary at equality.
2. **Follow-up:** a queued Deliver op starves the EvacResumer. The
   exclusive claim treats ANY queued op as a busy lane, so an Evacuating
   session that holds an undelivered prompt is never claimed — evacuation
   starves. Needs a due-gated exclusive claim (with the ADR 0098 D4
   conformance pass) or a Deliver verb that releases the lane on
   Evacuating.
3. **Follow-up:** the Deliver verb's Evicting rung-ascent cancels the
   queued evict op on every retry, so an Evicting session whose VM is
   gone never accumulates the attempt budget that routes it to the
   HostLost fallback (issue #762 path).

Until (2) and (3) land, the swarm's prompt step keeps its synthetic
in-step ack and the attach plane's Idle announcement is scenario-opt-in;
the attach gating, dial faults, and both oracles are always on.

## Rollout

One PR carries this ADR (Proposed) plus A1–A5, B, the interrupt-attribution
fixes, the held-echo UX fix, and the E oracles for the shipped pieces. Each
logical change is its own commit.

A6 (the prompt rides `start_agent`; needs the in-guest consumer, a harness
re-bake, and the exactly-once oracle) and A7 (vsock severance watch,
ping/pong liveness, the Firecracker RX-gate arm) land as the follow-up
phase. That phase flips this ADR to Accepted.
