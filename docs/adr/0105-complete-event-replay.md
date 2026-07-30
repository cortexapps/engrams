# ADR 0105 — the event stream replays the whole log

Status: Proposed

## Words used in this document

- **The log** — the `session_events` table. It is permanent. It holds every
  event of a session. It never loses an event.
- **The bus** — an in-memory broadcast channel in the coordinator. It holds
  only the newest 256 events. Older events fall out. It exists to push new
  events to clients quickly.
- **A page** — one read of the log. A page has a maximum size.
- **The cursor** — the highest event index that the stream sent to the
  client.
- **To walk the log** — to read page after page until the stream reaches
  the end of the log.
- **To tail the bus** — to forward each new event that arrives on the bus.
- **A lag** — the bus dropped events because the reader was too slow.

## Context

Clients read a session transcript through the app-gRPC `StreamEvents` RPC.
The web thread uses it. Slack watchers use it. CLI watchers use it. They all
call `api::events::events_core`.

That function read the log one time only:

```rust
const REPLAY_LIMIT: i64 = 1000;
let replayed = meta.list_session_events_since(id, since, REPLAY_LIMIT).await?;
```

The SQL is `WHERE idx > $2 ORDER BY idx LIMIT $3`. The order is ascending.
Therefore the read returns the oldest page. It drops the rest of the log.

`merged_event_stream` then set a boundary at the last event it read. After
that it forwarded only bus events above that boundary.

This makes a hole. The events after the first page have two problems. They
are too old to stay on the bus. They are past the limit of the read. So
nothing asks for them again.

### The prod incident

We found this defect on 2026-07-30. The session was
`eafd98b5-f748-484e-a27b-ed6de8343204`. It had 1291 events. We measured the
live control plane:

| request | frames | last index |
|---|---|---|
| `since=-1` | 1000 | 999 |
| `since=999` | 291 | 1290 |

The transcript stopped at index 999. That event happened at 03:58:18Z. The
last visible agent message was index 996. The true last agent message was
index 1269. That message summarized the run that opened the pull request. 47
agent messages were unreachable.

The user reported the defect as "a bunch of messages went away". A reload
truncated the page. The user could not tell this apart from data loss.

This is not the ADR 0028 rewind path. All 1291 rows had a NULL `rewound_at`.
This difference is important. We usually examine the rewind path first when
a transcript loses its end.

### Scale

At that time 6 of 688 sessions had more than 1000 events. The largest
session had 3258 events. The count is small. But these are the long
sessions. Their history has the most value.

## Decision

One rule governs the stream:

> The log is the truth. The bus is only an accelerator. The stream re-reads
> the log from the cursor whenever it cannot prove that it holds every
> event.

`merged_event_stream` becomes a loop with two states. It keeps one item of
state: the cursor.

**The catch-up state.** Read events above the cursor. Use pages of
`REPLAY_PAGE`. Send each event. Move the cursor to that event's index. A
short page means the stream reached the end of the log. Then change to the
tail state. `REPLAY_PAGE` is a page size. It is not a ceiling.

**The tail state.** Forward a bus event only when its index is exactly
`cursor + 1`. Then move the cursor. Drop a bus event at or below the cursor,
because the stream already sent it.

An index above `cursor + 1` is a jump. The stream does not send it. Instead
the stream returns to the catch-up state and reads the log.

### The bus is not ordered. The log is.

Two paths publish to the bus. `AppState::emit` publishes a local commit at
once. `pg_listener` publishes a commit from another replica later, after a
`LISTEN/NOTIFY` round trip. Production runs two coordinator replicas.
Therefore one replica can see its own index 12 before the notification for a
peer's index 11.

The log has no such problem. `append_session_event` allocates an index in one
autocommit statement. That statement takes a row lock on `sessions`.
Therefore appends to the same session serialize, and commit order equals
index order. A visible index 12 proves that index 11 is also visible.

This is why the jump returns to the log. If the stream sent index 12 and
moved the cursor to 12, then index 11 would be lost forever. The late bus
copy of 11 would look already-sent. A client that reconnects at
`Last-Event-ID: 12` would also skip 11.

The walk sends the whole range in order. So the cursor keeps its true
meaning: the stream sent every event up to the cursor.

**The drop rule also removes a duplicate.** `pg_listener` re-broadcasts on
every replica, including the replica that produced the event. So each local
event reaches the bus two times. The cursor drops the second copy. The old
code compared against a constant, which it set one time at the start of the
stream. Therefore both copies passed, and the client got the event two times.

Phase 1c chunks (ADR 0052) are different. They never enter the log. They
hold no real index. Therefore they skip the cursor test. They also must not
move the cursor.

A lag returns the loop to the catch-up state. The bus dropped events that
the stream never sent. The log still holds those events. So the stream
reports the lag, and then it walks the log again from the cursor.

This also repairs an older defect. Before this ADR the stream reported a lag
and then did nothing. The client kept a permanent hole until it
reconnected. The catch-up state repairs the hole. **One catch-up state
serves both the first walk and the walk after a lag.**

A walk after a lag sends `MergedEvent::Replay`, not `MergedEvent::Live`.
Therefore those events carry their true `recovery_epoch` and `rewound_at`
from the log. The `Live` arm writes `(0, false)` instead. So the new
behavior is more correct.

### Why the lag repair is necessary

The stream subscribes to the bus before it reads the log. This order is an
older rule. It stops the stream from losing events that arrive during a
read.

That rule has a limit. The bus holds 256 events.
`SessionEventBus::default()` sets this depth.

Before this change the stream made one read. The read took milliseconds.
Two or three events queued up. There was no risk.

A walk takes longer than one read. It makes many reads. The client must also
receive each page. On a long session with a busy agent, more than 256 events
can arrive during a walk. Then the bus drops its oldest events.

So a walk without the lag repair replaces one known defect with a worse
one. The lag repair is part of the fix. It is not an extension of the fix.

### A failed read is honest

A read can fail on the second page. That failure must not look like the end
of the stream. A truncated stream that looks complete is the defect that
this ADR removes.

Therefore the stream item becomes `Result<MergedEvent, ApiError>`. A failed
read sends `Err` and then ends. The gRPC handler maps `Err` to a `Status`.
The client reconnects with `Last-Event-ID`. The walk restarts from the
client's own cursor.

ADR 0103 states the same doctrine. An honest retryable error is better than
a false clean end.

`events_core` still calls `get_session` first. Therefore an unknown session
still fails the RPC at once. It does not fail in the middle of a stream.

## Consequences

**Memory per client drops.** The stream holds one page. It no longer holds a
buffer of 1000 events. That is about 750 KB instead of about 1.5 MB. We
measured a mean payload of about 1.5 KB. This direction is important,
because the coordinator has a memory problem. Its limit is 2Gi. It was
`OOMKilled` many times on 2026-07-30. We track that cause separately.

**The largest client load** is the largest session. That is 3258 events, or
about 5 MB of JSON. This is acceptable. Therefore we do not add a window to
the user interface.

Sessions could grow much larger later. Then the client must ask for a window
with `since`. The server must not choose a window without telling the
client. Silence caused this incident.

**A busy session can walk more than one time.** An agent could make events
faster than the stream sends them. Then the loop repeats: lag, walk, lag,
walk. Each walk moves the cursor forward. Therefore the stream always makes
progress, and the client gets every event. This costs extra reads. It does
not stall.

**The wire format does not change.** The frames are the same. The kinds are
the same. The `id:` rule is the same. So there is no proto change, no
`WIRE_VERSION` change, no image re-bake, no host roll, and no skew window.
Deploy the coordinator only. The changed files are two coordinator source
files, two test files, and this document.

**We retired `tokio_stream::wrappers::BroadcastStream` here.** The loop calls
`recv()` instead. `recv()` reports a lag directly.

## Alternatives rejected

- **Raise `REPLAY_LIMIT`.** This moves the cliff. It does not remove it. A
  silent truncation at any number is the defect.
- **Read the whole log into one `Vec`.** This is the smallest change. But
  then memory per client holds the whole transcript. That is the wrong
  direction for a pod that already dies of memory exhaustion.
- **Change the web client only.** The client could read through the
  paginated `ListSessionEvents` RPC, which already walks correctly. But then
  Slack and every other client still truncate. The defect belongs to the
  shared function.
- **Remove the lag frame.** The walk now repairs the hole. But the client
  still wants to know that its live feed fell behind. The frame carries no
  index, so it costs no cursor state. We keep it.

## Testing

The tests match the failure modes above. We drive `REPLAY_PAGE` with real
volume. We do not add a test-only page size, because this repo rejects
test-only hooks.

Unit tests in `api::events` use `SimMetadataStore`:

| test | it asserts |
|---|---|
| `replay_walks_past_one_page` | 1200 events in, 1200 events out, in index order |
| `replay_to_live_seam_is_gap_free_and_dup_free` | an event that arrives during a walk appears one time |
| `failed_page_read_is_terminal_not_clean_eof` | a failed read sends `Err`, not the end of the stream |
| `bus_lag_backfills_from_the_log` | a lag repairs the hole, with no gap and no duplicate |
| `out_of_order_bus_event_walks_instead_of_skipping_the_hole` | a jump reads the log and sends the range in order |
| `listen_echo_of_a_local_emit_is_dropped` | the second copy of a local event never reaches the client |
| `ephemeral_chunk_bypasses_the_cursor_gate` | a chunk skips the cursor test and does not move the cursor |

Each `next()` call has a timeout. This is deliberate. A short walk leaves
the stream in the tail state. Then an unbounded `next()` waits forever. A
test that waits forever reports a timeout. It does not name the defect.

**We did two red checks.** First we set the walk back to one read
(`catching_up = false`). Then
`replay_to_live_seam_is_gap_free_and_dup_free` failed, and both walk tests
stopped at 500 events. Second we set the tail test back to `idx > cursor`.
Then `out_of_order_bus_event_walks_instead_of_skipping_the_hole` failed.
Therefore the tests are not vacuous.

`listen_echo_of_a_local_emit_is_dropped` passes under both tail rules. It
guards a different regression: a return to a constant boundary, which the
code used before this ADR.

### Review history

A Codex review found the out-of-order defect. The first version of this
change moved the cursor to any bus index above it. That version fixed the
1291-event incident but lost events in a two-replica deployment. We confirmed
the report against the code, and we confirmed the index-allocation order that
makes the repair correct. The repair also removes the duplicate that
`pg_listener` produces on the replica that emits an event.

End-to-end test in `crates/engram-coordinator/tests/grpc_app.rs`:
`session_stream_events_replays_past_one_page` calls the real `StreamEvents`
RPC through a live tonic server. It uses 1200 events. It asserts that every
index arrives. The unit tests hold the loop. This test holds the wiring:
handler, then `events_core`, then `merged_event_stream`, then proto frames.
The prod defect appeared at that wiring.

Conformance test in `crates/engram-sim/tests/meta_conformance.rs`, named
`t_list_session_events_pages`. It walks `list_session_events_since` from the
last index that the store returned. It asserts no gap and no duplicate. It
also asserts ascending order, a limit that the store never exceeds, an
exactly-full last page that ends the walk, and an empty read at the end.
Both stores run this test: the sim store and real Postgres.

That scenario pins the real SQL. The store method does not change, so ADR
0098 D4 does not demand this test. But the new walk depends on the page
boundary. The sim store uses `.take(limit)`. Postgres uses SQL `LIMIT`. A
difference between them would break the walk in one store only. So the
scenario earns its place.

We checked that the Postgres half is not vacuous. We made an assertion
wrong on purpose. The `pg` half failed. Therefore it runs the body. It does
not skip on a missing database.

**CI needs no change.** The conformance Postgres step already runs
`-p engram-sim --run-ignored ignored-only` over the whole package. The unit
tests and the `grpc_app` test do not carry `#[ignore]`. Therefore the normal
workspace lane runs them.

## Commit chain

(we fill this in when the status becomes Accepted)
