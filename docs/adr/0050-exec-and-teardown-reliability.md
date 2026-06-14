# ADR 0050: exec + teardown reliability — no swallowed streams, no leaked sandboxes

**Status:** Accepted (2026-06-14) — shipped as `d0d5188e` (ADR), `273dfc36`
(coord-side B+C+E-retry), `2d7928ff` (host-side D+E); engrams-internal loadtest
`7a7df94` (A). **Prod-validated**: the n=30 load test on a clean `kvm` fleet
went **30/30 lossless** (was 25/30), **0 "no live sandbox"**, **0 genuine data
loss**, and **0 leaked firecracker VMs after settle** (was 46) with the fleet
scaling 2→5→2 and the queue draining — the headline acceptance gate. The ADR 0048
fleet load test, re-run at `replicas: 2` after the ADR 0047 follow-up (delete
`SandboxRegistry`), had come back with **0 "no live sandbox" 409s** — the
multi-replica routing bug was fixed — but surfaced a cluster of transient-gRPC
reliability gaps that all trace to one cause: **the coordinator treats its gRPC
channel to a host as reliable, and mishandles it when it isn't.** Three symptoms,
one root cause:

| Symptom (load test, n=30) | What happened | The gap |
|---|---|---|
| 1× exec `500` (`tcp connect error`) | first RPC to a just-scaled host failed to connect | exec dispatch has **no retry** |
| 4× "truncated workload output" | the exec stream dropped before the `Exit` event | exec returns **`200` + partial stdout, `exit_status: None`** |
| **46 leaked firecracker VMs** | `destroy` RPC failed during teardown → FC never died | **no guaranteed teardown** + nothing reaps it |

**Related:** ADR 0047 (stateless coordinator — the `replicas: 2` move that
exposed fresh-host races), ADR 0009 (heartbeat reconcile — sweeps the *opposite*
direction), ADR 0013 (HTTP heartbeat), ADR 0015 M3 (`resolve_owner` read-through
routing), ADR 0045 C1 (the migration ownership-rule this generalizes).

## Context

`engram-protocol`'s `GrpcHostPool` dials hosts with `connect_lazy` (the first
RPC pays the connect cost) and documents — correctly — that "no retry logic
lives here … per-RPC retry is the call-site's job." But the exec call site
(`HostRegistry::exec_stream`) doesn't retry, and the exec handler doesn't
distinguish a clean process exit from a dropped stream. So a freshly-scaled
host (whose gRPC server is up but whose channel this coord pod hasn't dialed,
or which is briefly unreachable under boot load) produces either a hard `500`
(connect failed) or a *silent* truncation (`200` with the process's first lines
and no exit status).

The leaked VMs are the same failure on the **teardown** path. Every `destroy`
is a coordinator-driven, best-effort, fire-and-forget RPC; on failure the code
warns and continues, and — worse — `delete_session` / `idle_evict` then
**clear `sessions.sandbox_id`**, erasing the only durable pointer to the
still-running FC. The sandbox is now invisible to every reconcile and leaks
forever. The `"host-agent reconcile will GC"` comment in `delete_session` was
aspirational: the only ownership sweeps that exist are migration-specific
(export-TTL, reattached post-copy source). ADR 0009's reconcile sweeps the
*other* direction (session-alive → sandbox-missing → `HostLost`), never
sandbox-alive → session-gone. Across the load-test runs, every failed teardown
leaked one FC; 46 accumulated, burning ~14 GB of host RAM.

## Decision

Five changes. The live path (A–D) stops *creating* the failures; the teardown
path (E) makes destruction *guaranteed* so a zombie can never form, with a
host-local reaper as the floor.

### A. Diagnostic — distinguish the failure (load test, `engrams-internal`)
The load test discarded the exec **exit status** and **stderr** — exactly the
discriminator. Capture them: `exit != 0` ⇒ the in-guest write genuinely failed
(stderr names ENOSPC / OOM); `exit == None` ⇒ the stream dropped (B's bug).
Same "validate what the failure *is*" discipline that killed the corruption
phantom.

### B. Exec stream integrity (`api/exec.rs`)
The exec event loop only records `exit_status` on an explicit `Exit` event. If
the stream ends *without* one, that's a dropped connection, not a clean exit —
return a typed `502 exec stream truncated` instead of `200`-with-partial-stdout.
The coordinator must never report a cut stream as success. (Streaming
`exec_stream` emits a terminal error event in the same case.)

### C. Typed transient error + bounded retry (`SandboxError`, `grpc_client`, `HostRegistry`)
Add `SandboxError::Unavailable` (transient, retryable), and map tonic
`Code::Unavailable` (connect failure / channel evicted) onto it in
`grpc_client` instead of collapsing it into `Vm(BoxError)`. `HostRegistry`'s
exec dispatch retries `Unavailable` a bounded number of times (re-resolving the
owner each attempt, short backoff) — the per-RPC retry the pool defers to the
call site. Exhausted retries surface as a retryable `503`, not a `500`.
(Mid-stream retry stays unsafe for non-idempotent execs — B surfaces that case
as an error and lets the client decide.)

### D. gRPC readiness gate (`engram-host-agent`)
A host must not become *schedulable* before its gRPC server is actually
serving. `grpc_server::boot` binds its listener and signals readiness *before*
entering `serve`; host startup withholds the **first heartbeat** (the row that
makes the host visible + schedulable) until that signal fires. A just-joined
host therefore never receives a placement or exec it can't yet answer — closing
the window that produces C's connect failures in the first place.

### E. Guaranteed teardown — host-local, never-forgotten (`engram-host-agent` + coord)
The teardown path stops being best-effort:

1. **Inline retry.** Coordinator `destroy` reuses C's retry on transient
   `Unavailable`, so the common blip completes immediately (the happy path
   never lingers an FC).
2. **Host-local reconcile (the guarantee).** A new periodic host-agent sweep —
   generalizing the migration source-ownership rule (ADR 0045 C1) to *all*
   sandboxes — walks `backend.list()` each tick: for a sandbox not in a
   migration role and past a debounce, it asks the coord
   `sandbox_ownership(session, sandbox)` (the endpoint already exists; it
   answers `sessions.sandbox_id == this` from PG). `owned == false` (session
   terminal / gone / rebound away) ⇒ **`destroy()` locally**. A local destroy
   can't be defeated by the same coord→host gRPC flakiness that leaked the FC.
   A debounce (N consecutive orphan verdicts) and a min-age grace keep an
   in-flight `create` (binding not yet published) from being reaped;
   coord-unreachable ⇒ keep (never destroy on a transient ownership-check
   failure).

`sandbox_ownership` reads `sessions.sandbox_id` (PG), which ADR 0047 just made
the sole authority — so a terminal/idle/rebound session reliably answers
"not owned" on every replica, and the host reaps host-locally. The ADR 0009
reconcile (session → sandbox-missing) and this reconcile (sandbox → session-gone)
are now both covered.

## Consequences

- A fresh-host burst no longer produces hard `500`s (D removes the window; C
  retries the residue) or silent truncations (B). Failures that *do* surface
  are typed and honest (`502 truncated`, retryable `503`).
- Zombies cannot accumulate: the inline retry clears the happy path, and any
  residue is reaped host-locally within a reconcile interval — the destroy
  runs where it can't fail over the network. `running_sandboxes` tracks reality.
- One extra per-host background task (the reconcile) and one per-RPC retry
  budget on exec. Negligible against the work they guard.
- `SandboxError::Unavailable` is a new variant — exhaustive matches across the
  workspace get an arm (mechanical; clean break, zero users).

## Validation

- Unit: exec returns `502` on a stream that ends without `Exit`; `HostRegistry`
  exec retries then succeeds / surfaces `503`; the reconcile keeps owned + a
  young sandbox and destroys an orphan past debounce; readiness gate withholds
  the heartbeat until the bound signal.
- FC integration: extend a two-host test so a `destroy` whose RPC is dropped is
  reaped by the next host reconcile (`running_sandboxes → 0`).
- Prod: re-run the ADR 0048 load test (n=30) on the clean fleet — expect
  30/30 lossless (or honest typed failures), and **0 leaked FCs after the run
  settles** (the headline acceptance gate). Flip to Accepted on a clean run.

  **Result (2026-06-14, n=30 on a clean 2-node `kvm` fleet, coord `replicas: 2`):**
  30/30 OK / 0 FAIL, 0 "no live sandbox", 0 genuine data loss; fleet scaled
  2→5→2 with the queue draining FIFO; settled to 2 hosts with **0
  `running_sandboxes`** (the zombie gate). The four prior-run failures (4×
  truncated output, 1× connect 500) did not recur. Accepted.
