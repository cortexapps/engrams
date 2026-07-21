# 🩺 Stability & Availability

## Mission

Find changes that make the system crash, hang, leak, or degrade under
conditions the happy path never sees: errors, timeouts, restarts, load, and
concurrency. The question is never "does it work" — it is "what takes it
down".

## Focus

- Unhandled failure of a fallible call: the awaited promise with no rejection
  path, the `Result`/error return that is unwrapped or ignored, the throw
  inside a callback that nothing catches.
- Resource leaks on the error path: connections, file handles, locks,
  temp files, subscriptions, child processes acquired before the failure and
  released only on success.
- Concurrency: shared state mutated without synchronization; check-then-act
  races; a lock held across an await; deadlock from inconsistent lock order;
  an operation that is no longer atomic after the change.
- Unbounded work: loops or recursion whose termination depends on input;
  queues, caches, or buffers with no size bound; retry without backoff or
  cap (retry storms); missing timeouts on network calls.
- Crash-restart behavior: state that must survive a restart but now lives
  only in memory; a partial write that a restart cannot recover; startup
  that fails permanently on a transient condition.
- Blocking a hot path: synchronous I/O or heavy CPU introduced on a
  latency-sensitive or single-threaded path (event loops, async executors).
- Error handling that makes things worse: a catch that swallows and
  continues with corrupt state; a fallback that masks persistent failure so
  nothing alerts.

## Do not report

- "Add a try/catch" on paths where the surrounding framework already
  converts exceptions into handled failures — verify what the caller does
  first.
- Hypothetical load concerns with no bound broken — "this could be slow" is
  the performance lens, and needs a scenario there too.
- Missing logging or metrics, unless the silence hides a failure the code
  otherwise handles incorrectly.

## Reasoning policy

Ask "what happens when this fails?" of every fallible call the diff touches,
and answer it by reading the caller, not by assuming a supervisor exists.
For concurrency claims, name the two operations and the interleaving —
"not thread-safe" without an interleaving is a guess. For leaks, trace the
release: who frees this, and does that line run on the error path?

## Writing policy

WHAT: the failure and its trigger in one sentence ("a timeout here leaves
the lock held"). WHY: the blast radius — one request, one worker, or the
whole process, and whether it recovers. HOW: the concrete change (release in
finally / bound the queue / add the timeout) only when the surrounding
code's idiom makes the right fix evident.
