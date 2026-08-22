# Runbook: the Slack thread-brain engine window (ADR 0119 phase 4.6)

The Slack thread brain exists twice during the parity window:

- **Legacy**: the per-thread DBOS workflow `slack-thread.ts` (ADR 0060).
- **Engine**: the `slack_brain` built-in automation — a definition on the
  block engine with two system blocks (`system.slack_thread_relay`,
  `system.slack_thread_recap`).

One channel is served by exactly one of them. The integration dispatcher
decides per delivery, and the Slack events route
(`orchestrator/src/routes/slack-events.ts`) follows that decision:

```
dispatcher admitted a run for slack_brain (started / joined) → engine (legacy skipped)
anything else (not flagged, built-in disabled, kill switch)  → legacy (byte-identical to before 4.6)
```

"Flagged" means: the `slack_brain` built-in is **enabled** AND the channel id
is a key of its `channels` input — the dispatcher's trigger match
(`scope: { fromInput: "channels" }`). The route does NOT do a second
lookup: it reads the dispatcher's own admission result for the delivery
(`DispatchIntegrationResult.builtins`), so there is no cache, no replica
skew, and a flag lands on the very next delivery on every pod. The two
brains cannot disagree on who owns a delivery because only one read is
made.

The ingress spine ledgers and dispatches every Slack event regardless of
the window. The engine's trigger scope (`scope: { fromInput: "channels" }`)
is what keeps unflagged channels out of the engine: a channel that is not a
key of `channels` never matches, so no run starts. The route's branch is
the mirror image — it keeps the legacy workflow out of flagged channels.

## Flag one channel

Prerequisites: the seeder has run (the built-in row exists, disabled, with
an empty `channels` map), and a Slack connection exists (the seeder binds
the default one).

1. Find the channel **id** (`C…`, from the channel's details in Slack or
   the `channel` field of a ledgered event). Use the id, not `#name`: the
   route and the trigger scope compare against the event's channel id.
2. Pick the profile the brain uses in that channel (a profile id from the
   Profiles page).
3. Open **Settings → Automations → Slack thread brain → Inputs** and add a
   row to **Channels**: key = the channel id, value = the profile id. Save.
   This is `AutomationService.SetInputs` with
   `inputs_json = {"channels": {"C0123456789": "<profile_id>"}, …}` — send
   the full inputs object; keys must be declared by the inputs schema.
4. If this is the first flagged channel, enable the built-in on its
   **Settings** tab (`AutomationService.SetAutomationEnabled`,
   `enabled: true`). Until the row is enabled, flagged channels stay on
   legacy.
5. Confirm in the pod log on the next mention in that channel:
   `slack: app_mention → thread-brain built-in (legacy workflow skipped)`.
   An unflagged channel keeps logging `slack: app_mention → thread workflow`.

To take a channel back off the engine, remove its row from **Channels**
(or disable the built-in to take every channel back at once). The active
run for an open thread finishes on its own — its idle timeout ends it, the
session is kept — and the next mention in that thread goes to legacy.

## The two brakes

- **Per-channel**: remove the channel from `channels`, or disable the
  built-in. Takes effect on the next delivery on every replica (the
  dispatcher reads the row per delivery; there is no cache).
- **Kill switch**: `ORCHESTRATOR_SLACK_AUTOMATION_DISABLED=1` (or `true`;
  any other spelling is off). Every channel takes legacy, flags untouched,
  and the flag is never consulted. This is an orchestrator env var: add it
  to the orchestrator ConfigMap values and roll the deployment. The flags
  survive, so lifting the switch re-opens the same window.

The ingress spine keeps ledgering events in `integration_event` either way
(the ledger is the audit trail, not a brain). The kill switch gates BOTH
halves: the Slack route falls back to the legacy thread workflow, and the
integration dispatcher refuses to admit runs for the `slack_brain` built-in
(`disabledBuiltinsFromConfig` in `automations/dispatch.ts`) — so a flagged
channel is never served by two brains. You do not need to clear `channels`
to use the switch; the flags survive, so lifting it re-opens the same window.

## What to watch

- **Runs**: Settings → Automations → Slack thread brain → Runs. One run per
  thread (concurrency key `team:channel:thread_ts`, policy `join`). A
  healthy thread: `facts` → `admit` → `session` → `relay` → `first_turn` →
  `thread[n].next` … and ends `completed` when the idle timeout expires
  (the `until` exits the loop). `filtered` = the admission code rejected
  the event (bot message, top-level message, no profile for the channel).
- **Recap**: the finalize hook `__finalize__.recap` posts ✅ (completed),
  ❌ (failed / deadline) or a neutral note (halted / superseded) through
  the same Slack policy as legacy. `posted: false, rendered_as: "none"`
  means the relay never installed (e.g. `filtered`), which is expected.
- **Sessions**: `keepOnFinish: true` — a thread's session outlives the
  run. Expect kept sessions to accumulate; this is by design (D8).
- **Pod log**: the route line above, plus `integration_event` dispatch
  lines. Two brains on one thread (double ⏳ reactions, two answers) should
  be impossible; if you see it, the dispatcher gate has regressed — set the
  kill switch AND disable the built-in, then file it.
- **Inputs**: `idle_timeout` (default 3600 s) ends a quiet thread's run;
  `max_turns` (default 50) bounds follow-ups one run answers. Both are
  read at run time (`$ref`), so a change applies to the next wait of
  runs already in flight.

## Slack parity checklist

Judge the window against the legacy workflow on these, in a flagged test
channel before any real one:

- [ ] `@bot …` in a channel → ⏳ on the mention, a session with the
      channel's profile, the bot's replies stream into the thread.
- [ ] A follow-up reply in the same thread → delivered as the next prompt
      to the SAME session (no second session, no second run).
- [ ] AskUserQuestion → Block Kit in the thread; the answer reaches the
      session (the relay consumes `slack_answer`).
- [ ] The thread goes quiet for `idle_timeout` → the run completes, ✅ recap
      posts, the session stays open in the UI.
- [ ] A failed turn → ❌ recap with the error; the session is kept.
- [ ] A bot message / an edited message / a top-level channel message →
      `filtered`, no session.
- [ ] A mention in an UNFLAGGED channel → legacy behaves exactly as before
      (no engine run beyond the ledgered event; a run may show `filtered`
      if the channel has a `default_profile` — set `default_profile` only
      when every channel should be served).

Known v1 divergences (by design, see `builtins/slack-brain.ts`): the prompt
is the message text with the bot mention stripped, not the legacy
`<thread context>` fold of the whole thread; the LLM profile picker and
the "which profile?" dropdown are retired in favour of the `channels` map.

## Rollback

1. Remove the channel(s) from `channels` (or disable the built-in). This
   alone is enough; in-flight runs end on their idle timeout.
2. If the route or the engine misbehaves in a way the flag does not
   contain, set `ORCHESTRATOR_SLACK_AUTOMATION_DISABLED=1` and roll the
   orchestrator. Every channel is back on legacy, flagged or not.
3. A run that must stop now: Runs → the run → Stop (`StopRun`). The
   session is kept.
