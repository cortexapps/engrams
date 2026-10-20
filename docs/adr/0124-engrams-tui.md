# ADR 0124: `engrams tui`, a terminal client for sessions

Status: Accepted (2026-10-20); PR4-PR9 implemented in #1658 (one PR, the
phases as its commit groups; see Phases).

Terms used in this document:

- **TUI**: the `engrams tui` subcommand. It attaches a local terminal to a
  remote session and renders the conversation, tool calls, and status.
- **Transcript**: the ordered list of cells the TUI shows for one session.
  A **cell** is one rendered unit: a user prompt, an assistant message, a
  tool call, a status row.
- **Fullscreen mode**: the TUI owns the whole terminal through the
  alternate screen. It virtualizes the transcript and draws only the
  visible rows.
- **Native scrollback mode**: the TUI does not use the alternate screen.
  Finalized cells are written into the terminal's own scrollback. A small
  viewport at the bottom holds the composer, the footer, and the in-flight
  cell. The flag is `--no-alt-screen`.
- **Reducer**: the pure function that folds session events into cells. It
  mirrors `web/src/components/session-thread/buildMessages.ts`.
- **Wrap cache**: the per-width layout of one cell: a row count and, for
  each row, a byte range into the cell's text plus style runs.

## Summary, in plain English

The product CLI is Rust (`crates/engram-cli`, on `crates/engram-app-client`).
The TUI is a third crate, `crates/engram-tui`, that the CLI dispatches to.
It talks to the orchestrator only, with the same transport the CLI uses:
Connect unary calls for history and actions, SSE for live events.

The design copies what Claude Code and Codex CLI converged on: an app-owned
transcript by default, an escape hatch to native scrollback, `Esc` to
interrupt, `Enter` queues while a run is active, numbered approval
dialogs, and markers that carry meaning without color.

Two budgets drive every decision: memory and latency. The budgets are
tests, not goals. A change that breaks one fails `cargo nextest run`.

## Decisions

### D1. One crate, one reducer, two screen modes

`engram-tui` is a library crate. `engrams tui [session-id]` in `engram-cli`
calls `engram_tui::run(Clients, Options)`. The crate has one `App` state
and one reducer. The screen mode is an enum chosen at start:

- `Fullscreen` (default): alternate screen, `Viewport::Fullscreen`.
- `Inline` (`--no-alt-screen`, `ENGRAMS_TUI_NO_ALT_SCREEN=1`): ratatui
  `Viewport::Inline(h)` plus `Terminal::insert_before` with the
  `scrolling-regions` feature. Each finalized cell is inserted above the
  viewport once, pre-wrapped at the current width, and then dropped from
  memory. The terminal owns that text from then on.

The reducer, the wrap cache, the composer, and the keymap do not know the
mode. Only the frame function differs.

### D2. The reducer mirrors the web transcript

The web reducer is the truth table for which event kinds render and how.
The TUI reducer keeps the same rules: a user echo with a `prompt_id` is held
until its `run_started` or `prompt_steered`; assistant text is coalesced per
run; a `tool_call_completed` closes the open tool with the same id; an
inactive session status closes an open run. Kinds the web does not render
(`status_changed`, `evicted`, `teleport_finished`, …) stay out of the
transcript and only update the footer.

The reducer is incremental. Each event mutates the cell list in place. A
full rebuild only happens on a history backfill above the loaded window.

### D3. The transcript store holds text once

Each cell stores its text once, as `Box<str>`. The wrap cache stores byte
ranges and style runs, never a second copy of the text. The cache is
computed lazily on first visibility and dropped on resize. A prefix sum of
cell heights gives scroll math in O(log n). Rendering touches only the
cells that intersect the viewport.

While a message streams, only the uncommitted tail paragraph is re-wrapped
per frame. `agent_message_chunk` deltas append to one `String` and are
replaced by the final `agent_message`.

### D4. The event loop never ticks

A single-threaded tokio runtime runs three tasks: terminal input, network
(SSE plus RPC futures), and the app loop. The app loop `select!`s on
channels and draws at most once per 8 ms, inside a synchronized update.
There is no periodic timer. A spinner timer is armed only while a run is
active.

### D5. Budgets are tests

| Metric | Budget | Test |
|---|---|---|
| Allocations per steady-state frame | 0 | counting allocator in `tests/perf.rs` |
| Frame time, 200×60, 10k cells, scrolled | p99 ≤ 2 ms | `tests/perf.rs` |
| RSS, attached, 10k events loaded | ≤ 40 MB | stub orchestrator, Linux lane |
| Startup to first frame | ≤ 30 ms | `ENGRAMS_TUI_TRACE` log, asserted |

Dependency allow-list: `ratatui` (default features off; `crossterm`,
`scrolling-regions`), `crossterm` (`event-stream`, `bracketed-paste`),
`tokio` (`rt`, `macros`, `sync`, `time`), `futures`, `pulldown-cmark`
(default features off), `unicode-width`, `unicode-segmentation`,
`serde_json`, and the client crate. Anything else needs a stated reason in
the PR. No syntax highlighting in v1.

### D6. History and reconnect

Open: `ListSessionEvents{before_idx: i64::MAX, limit: 200}` for the tail,
then SSE with `since = highest idx`. Scrolling to the top pages older
events with `before_idx`. Events are de-duplicated by idx. A lagged frame is
ignored; the server re-reads from the cursor on reconnect
(`Last-Event-ID`). Reconnect backs off from 0.5 s to 8 s and the footer
shows the attempt count.

### D7. Keymap

`Enter` sends; the server decides queue or steer. `Shift+Enter` and `Ctrl+J`
insert a newline. `Esc` interrupts a run or closes a popup; `Esc Esc` clears
the draft. `Ctrl+C` clears the draft; twice within 1 s quits. `Ctrl+D`
detaches to the session list. `Ctrl+P` opens the session palette. `Ctrl+O`
expands the last tool call. `PgUp`/`PgDn` and the wheel scroll; auto-follow
pauses while scrolled up.

## Consequences

- The TUI cannot show full tool output. The wire carries summaries only.
  That is a server limit and is out of scope here.
- Native scrollback mode cannot page history on scroll-up; the terminal owns
  the scrollback; the terminal's own search covers it.
- Zellij and ConPTY do not honor scroll regions reliably. Inline mode there
  uses the full-redraw fallback ratatui provides without the feature.

## Phases

The phases were built as a chain of stacked PRs and landed as one PR,
#1658, with the chain's commits in order. The phase names below are the
commit groups.

- PR4: this ADR, the crate core, both screen modes, session list,
  attach, composer, footer, keymap, tests and budgets.
- PR5: markdown writer, tool cells, file-change hunks, transcript
  search, `$EDITOR`, `NO_COLOR`, screen-reader flattening.
- PR6: approvals and questions via `CompleteToolCall`, notifications.
- PR7: shell pane (`vt100`) and `engrams session shell`.
- PR8: new session from the TUI, `/` commands, `/transcript` and
  `/dump`, docs, status → Accepted.
- PR9: the transcript's shape, after the first production sessions were
  read side by side with Codex's TUI (see the divergence log), and the
  fixes from an adversarial review of the whole chain.

The Rust CLI the TUI rides on landed first (#1644, #1645). One transport
bug surfaced on the first run against a production deployment and was
fixed on main before this chain merged (#1652): an empty-body POST needs
an explicit `Content-Length: 0` through some front ends.

## Divergence log

- **PR4, RSS budget.** The RSS gate against a stub orchestrator was not
  built: a `tests/perf.rs` counting allocator measures the live heap of
  the 10k-event transcript in-process instead (measured 2.4 MB live for
  1.9 MB of raw events; asserted below 1.5× raw + 1 MiB), which is the
  quantity D3 controls. RSS of the real binary was measured by hand in a
  pseudo-terminal: 7.1 MB fullscreen, 8.0 MB native scrollback, on the
  session list. The stub-driven RSS gate stays a follow-up.
- **PR4, frame budget.** Measured p99 0.32 ms per frame at 200×60 over a
  5.4k-cell transcript in release (2.5 ms in debug); one allocation over
  200 steady frames, from ratatui's diff vector, not from drawing.
- **PR4, inline viewport height.** ratatui fixes an inline viewport's
  height at creation, so native scrollback mode reserves 14 rows. The
  composer grows to 6 rows inside it; the rest shows the in-flight cells.
- **PR4, history paging.** Only fullscreen mode pages older history
  (D6). In native scrollback mode the first page is written to the
  terminal and older events are not loaded; the pager overlay in PR5
  covers search.
- **PR5, links.** A ratatui cell carries no hyperlink, so links render as
  `text (url)` with the target dimmed, not as OSC 8. The URL is on screen
  and the terminal's own URL detection still makes it clickable.
- **PR5, streaming.** Chunks are rendered up to the last blank line
  outside a code fence or table (Codex's rule); the tail is shown as plain
  text until it commits, and a durable `agent_message` renders in full at
  once. Once a cell is final the markdown source is dropped and only the
  display text and its style runs stay (D3: text once).
- **PR5, search instead of a pager.** Ctrl+T opens an in-place transcript
  search (Enter walks to older hits and wraps, the hit heads the viewport)
  rather than a separate pager overlay. Dump-to-scrollback and the
  transcript-in-`$EDITOR` view are deferred to PR8. In native scrollback
  mode the terminal owns the history, so Ctrl+T points at its search.
- **PR5, `$EDITOR`.** Ctrl+G hands the terminal to `$VISUAL`/`$EDITOR`
  (`vi` by default). The input stream is dropped first: crossterm's reader
  thread would otherwise steal keystrokes from the editor. A non-zero exit
  leaves the draft unchanged.
- **PR5, screen-reader mode.** `--ax` (or `ENGRAMS_TUI_AX=1`) forces native
  scrollback, no mouse, and word gutters (`you:`, `agent:`, `tool:`,
  `edit:`, `shell:`, `question:`, `plan:`, `note:`) in a 10-column gutter
  in place of glyphs; the `⎿` body gutter is dropped.
- **PR5, file changes.** A `file_changed` event re-renders the write/edit
  tool card in place as a diff (`+`/`-` lines, counts on the head row,
  collapsed like a tool body); the completion that follows leaves the
  diff alone, and a failed edit keeps its error card (ADR 0054 Flavor A).
- **PR6, decisions without a modal.** A pending question or plan takes
  the number keys, Enter and Tab only while the draft is empty; a draft
  in progress keeps every key, and Enter with a draft sends it (the
  harness queues it). Plans need an explicit `1`/`2` before Enter, so a
  stray Enter never approves one. Feedback for a rejection is typed in
  the composer after Tab. Answers are `{question: [labels]}` and
  decisions `{decision, feedback?}`, the web's shapes, through
  `CompleteToolCall`; the cell locks optimistically and reopens when the
  RPC fails (the web's answered-set behaviour).
- **PR6, generic approvals.** Only `ask_user_question` and
  `exit_plan_mode` are deferred decisions on the wire today (as in the
  web); other `tool_call_requested` events render as tool cards.
- **PR6, notifications.** An unfocused terminal gets a bell plus OSC 9
  and OSC 777 when a decision starts waiting or a run ends; the title
  carries `⚠ needs you` / `⟳` while attached. Terminals that know neither
  sequence ignore them.
- **PR7, the shell socket.** `engram_app_client::shell` speaks ttyd's
  frames over the orchestrator's `tty` WebSocket. The socket verifies TLS
  the way the reqwest clients do (rustls, the platform trust store, via
  `engram_tls::client_config`), so the CLI still carries one TLS stack;
  tokio-tungstenite's `rustls-tls-webpki-roots` feature only unlocks the
  connector type, its bundled roots are not used. Keystrokes go as text
  frames when they are UTF-8 (the web's shape) and binary otherwise, which
  ttyd reads the same way.
- **PR7, one key for the pane.** Ctrl+` opens the shell and moves the keys
  to it; Ctrl+` again moves them back and leaves the pane open. Without
  the kitty keyboard protocol most terminals send NUL for Ctrl+`, so
  Ctrl+Space and Ctrl+@ are the same action. There is no prefix key: a
  shell program sees every other chord, Ctrl+C and Ctrl+D included. Esc
  from the composer closes an unfocused pane; after the remote shell exits
  Enter reopens a fresh one (the server never resumes a PTY).
- **PR7, pane size.** The pane takes two fifths of the fullscreen body,
  header included, and the transcript absorbs the composer's growth, so
  the emulated screen never resizes while a draft is typed. The size is a
  pure function of the terminal size, shared by the renderer and the
  resize path, so a resize frame goes out exactly when the screen changed.
  `vt100` keeps no scrollback: the pane is a window on the live screen.
- **PR7, native scrollback mode.** The pane is an alternate-screen overlay
  drawn through a second fullscreen terminal while the shell has focus;
  the inline terminal keeps its state and resumes when focus returns, so
  the scrollback above the viewport is untouched. Nothing from the shell
  is written into native scrollback.
- **PR7, `engrams session shell`.** The CLI verb is raw passthrough: raw
  mode, bytes both ways, `SIGWINCH` to a resize frame, and the process ends
  when the remote shell does. It has no escape key of its own.
- **PR8, new session.** Ctrl+N (or `/new`) is an overlay over either
  screen, not a screen of its own: a profile list from `ListProfiles`
  (filtered as you type), then a prompt composer, then `CreateTask` with
  the profile and the prompt. The TUI attaches to the task's primary
  session from the response and refreshes the list; a failed create keeps
  the prompt and shows the error. Harness, model and effort overrides are
  not offered; the profile's defaults apply, as in the web's quick path.
- **PR8, slash commands.** A draft of `/` plus letters is a command, not
  a prompt. One static table drives the popup above the composer, Tab
  completion (to the first match) and dispatch on Enter (an exact name,
  or the only prefix match; an ambiguous prefix leaves the draft and
  shows a note). There is no command history or argument parsing: every
  command is a verb with no arguments.
- **PR8, transcript export.** `/transcript` and `/dump` export the cells
  still in memory as plain text (tool bodies in full, never collapsed).
  `/transcript` reuses the `$EDITOR` hand-off from PR5 and discards the
  file. `/dump` leaves the alternate screen, writes the text to the main
  screen and comes back, so the terminal holds it as scrollback once the
  TUI exits (Codex's dump technique). In native scrollback mode `/dump` is
  a note: the transcript is already in the terminal. The export covers
  the loaded window only, not events never paged in.
- **PR8, `/plan`.** The web's plan chip has a TUI twin: `/plan` toggles
  the mode the next prompt asks for, and the prompt carries
  `harness_mode` only when it changes the session's current mode (the
  reducer's `mode`), as the web does. The footer shows `· plan` while it
  applies.
- **PR8, production findings.** Four defects surfaced only against a
  production deployment and are fixed in this PR: the harness emits a
  `tool_call_started` for a deferred question before the request, so the
  row rendered beside the card (now withdrawn when the request lands);
  the kitty keyboard protocol keeps one flag stack per screen, so the
  native-scrollback shell overlay (alternate screen) had no flags and
  Ctrl+` could not hand the keys back (the overlay pushes them too); a
  front end that ends the TLS session without `close_notify` when the PTY
  exits made `session shell` exit 2 with an error (an abrupt end is a
  normal end now); and an empty-body POST needed `Content-Length: 0`
  (#1652, on main). Measured on a 5k-event session: 13-16 MB RSS.
- **PR9, the transcript's shape.** The first production sessions read as
  a wall of text next to Codex's TUI, whose `history_cell` module was
  read for this pass. What changed, each in its own commit: a tool row
  shows the subject of its call (the command, the path, the pattern),
  not the argument JSON, and a long command folds to three rows; a blank
  row separates cells, with tool-like cells and markers sharing a block;
  a question the harness re-fires after a resume replaces its card
  instead of adding one, and an answered card keeps only `answer: …`
  lines; snapshot markers are gone (resumes and the two disk warnings
  stay); tables are fitted to the width (columns shrink to their longest
  word, cells wrap, then `header: cell` records), so a finished message
  with a table keeps its source to re-fit on resize, the one exception to
  D3's "drop the source"; consecutive Read, Grep, Glob and LS calls fold
  into one `Explored` row; diff rows lead with `Edited`, `Wrote` or
  `Patched`; a finished turn ends in `worked for 2m 14s` from the `at`
  stamps on its run events. Two more production findings: a result
  summary ends in a newline (now trimmed), and `file_changed` lands after
  the write's completion (the card keeps its id and is found among the
  newest cells). The footer lost its `idx` chip, a protocol number.
  Last, a finished turn folds its tool calls: each run of consecutive
  tool cells with two or more calls becomes one dim row (`▸ Ran 6 shell
  commands, read 3 files`) that a click or Ctrl+O opens, as Claude Code
  and Codex do. The fold is a property of the entries (the first carries
  the summary row, the rest have no rows while closed), so indices stay
  put; native scrollback mode does not fold, its finished cells have
  already left memory. A scroll past the top of memory used to leave a
  blank frame until the next event: the walk now clamps so the top page
  fills the area and runs again in the same frame, and a scroll that
  reaches the top of memory pages older history in. A prompt sits on a
  band across the row, as in Claude Code. A tool row whose argument JSON
  the server cut short keeps its subject (the first subject-like string
  field, unescaped up to the cut). The composer sits between two dim
  rules, as in Claude Code, so the input is set off from the transcript
  (one rule above it in native scrollback mode, whose viewport is short).
- **PR8, the budgets.** The RSS gate against a stub orchestrator (D5)
  stays unbuilt; the measured numbers are in the PR texts: 13-16 MB RSS
  attached to a 5k-event session in fullscreen mode, 14 MB in native
  scrollback mode, binary 4.2 MB. The in-process heap and frame budgets
  in `tests/perf.rs` are the enforced part.
- **PR4, cursor-position query.** An inline viewport needs the terminal
  to answer `CSI 6n`. A pseudo-terminal with no emulator behind it never
  does, and start-up blocks in crossterm's query. Real terminals answer.
