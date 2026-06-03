# Engrams Dashboard — UI Kit

A high-fidelity, interactive recreation of the **Engrams web dashboard** — the
coordinator-served operator console for the ephemeral-sandbox orchestrator.
This is a cosmetic recreation (real interactions, fake data) lifted from the
component code in `cortexapps/engrams › web/src/`, not production code.

## Run it

Open `index.html`. It's a single-page prototype with client-side routing
across three views. No build step — React + Babel load from CDN; the Google
Fonts (Newsreader + JetBrains Mono) load from CDN too.

## What's interactive

- **Overview** — live clock, vital-signs stat row, host manifest (expand
  "▸ cow state" on a host to see per-sandbox copy-on-write diagnostics),
  and the session manifest. Click **+ new session** to open the inline form,
  pick an image + mode (Claude images surface a credential field), and
  **start →** — it creates a session and navigates into it.
- **Session detail** — the agent transcript (assistant turns with margin
  role-labels + timestamps, bracketed tool-call asides, the quoted run
  boundary, the verdigris pull-request artifact). Type in the **prompt
  composer** and **send →** (or ⌘↵) to append a turn and get a canned reply.
  Switch to **SHELL** for a faux Solarized-Light terminal, or **RAW** for the
  indexed event log.
- **Settings** — open via the **§** mark (top-right) → Settings. Enable /
  disable images, see digests + manifest metadata; Registries + Profile stubs.

## Files

| File | What |
|---|---|
| `index.html` | Shell — loads fonts, CSS, and the JSX scripts in order. The four-surface app. |
| `colors_and_type.css` | The design foundation (copied from the system root). |
| `dashboard.css` | Component styling + small utility shims (the real app uses Tailwind v4). |
| `data.jsx` | Fake-but-realistic seed (hosts, sessions, a full transcript) + format helpers. |
| `store.jsx` | The mutable store with `createSession` / `sendPrompt` / `enableImage` / `disableImage`. |
| `components.jsx` | `StatusGlyph`, `VitalSigns`, `HostManifest`, `SessionManifest`, `TabRow`, `UserChip`, section heads. |
| `transcript.jsx` | `Transcript`, `ToolCall`, `PullRequestCard`, `RunBoundary`, `IdleMarker`, `PromptComposer`. |
| `screens.jsx` | `NewSessionForm`, faux `TerminalPane`, `ImagesPanel`, plus the reused `SessionDetail` / `Settings` pages. |
| `engram-mark.jsx` | The logo as a React component (`EngramMark` — static / pulse / loop). |
| `nav.jsx` | The persistent nav spine (wordmark + status mark + Sessions/Fleet/Storage/Settings tabs + chip). |
| `surface-sessions.jsx` | Sessions surface — vital strip + lifecycle-grouped manifest. |
| `surface-fleet.jsx` | Fleet surface — host strata (capacity bars, sandbox cells, drain) + reconciler. |
| `surface-storage.jsx` | Storage surface — durability ledger + chunk rollups. |
| `app-redesign.jsx` | Root component, router across the four surfaces + session detail, living-status wiring. |

All components export to `window` at the end of each file so the separately
-transpiled Babel scripts can share them.

## Fidelity notes

- **Source of truth:** every component mirrors a real one in
  `web/src/components/` — `Glyph.tsx`, `VitalSigns.tsx`, `Transcript.tsx`,
  `ToolCall.tsx`, `PullRequestCard.tsx`, `PromptComposer.tsx`,
  `NewSessionForm.tsx`, `TabRow.tsx`, `UserChip.tsx`, `CowState.tsx`,
  `HostManifest.tsx`, `SessionManifest.tsx`, `settings/ImagesPanel.tsx`, and
  the `Overview` / `SessionDetail` / `Settings` pages.
- **Simplifications:** the real app uses React Query + SSE + react-router +
  framer-motion + ghostty-web (a real WASM terminal). Here, data is a local
  store, motion is CSS, the router is a state switch, and the shell is a static
  Solarized-Light transcript. Behavior is cosmetic.
- **Left deliberately blank:** auth (the § mark is a placeholder), the
  Registries/Profile panels (stubs — the real ones manage KEK-sealed creds),
  and anything not present in the source.

## Reuse

Lift any component into a new design: copy the JSX file (and
`colors_and_type.css` + the relevant chunk of `dashboard.css`), keep the
`window` exports, and mount with the same CDN script order as `index.html`.

## Redesign — the four-surface IA

The kit IS the redesign: a green-field rethink of the information architecture
that splits the one do-everything Overview into four surfaces on a persistent
**nav spine**, separating *the work* from *the machine*:

- **Sessions** *(default)* — the driver's home. A compact vital strip, a
  primary "+ new session", and the manifest **grouped by lifecycle**
  (active → idle/resumable → archived) with ledger-section headers. Session
  detail drills in here (sub-crumb on the nav spine).
- **Fleet** — the operator's home. Hosts as first-class **strata**: each a
  ruled lane with a capacity bar, its running sandboxes as cells, draining
  controls, and a reconciler-status footer. A `Fleet view` tweak swaps strata
  for a dense table.
- **Storage** — COW state's real home. The chunk/durability layer as a proper
  diagnostics surface: fleet rollups (chunks, dedup, snapshots, unflushed,
  locality, GC) + a per-sandbox **durability ledger** (dirty chunks, unflushed
  bytes, base-chunk locality, RPO). No longer a toggle bolted onto a host row.
- **Settings** — images / registries / profile (reused as-is).

**Brand as living status:** the nav-spine `EngramMark` *strikes once* on each
poll tick and *loops* continuously while any session is booting/resuming
(replacing the old "polling every second" text). Booting/resuming sessions swap
their status glyph for the inline trace loader. The seeded booting session
resolves to active ~5s after load so you can watch the loader→resolved
transition and the mark settle from loop to static.

## Conversation redesign (session transcript)

A fresh pass on the session conversation, addressing two things the original
`Transcript.tsx` got wrong and surfacing events it dropped:

- **Clear turn-taking, without chat bubbles.** The human's prompt renders as a
  contained, square `paper-warm` panel marked with the `§` mark and a `you`
  label (`UserTurn`); the assistant stays as open prose in the reading column
  with its margin role-label. The contrast (contained prompt vs. open prose)
  reads turns as clearly as ChatGPT/Claude bubbles while staying true to the
  lab-notebook system (which deliberately rejects bubbles). The prompt is
  carried by `run_started.prompt_summary` — no duplicate user message.
- **Processes as first-class** (`Process`). `exec_started` / `exec_completed` /
  `stdout` / `stderr` — events the original transcript **dropped** — now render
  as shell-vocabulary lines: `● $ cargo nextest run … · exit 0 · 18s ▸`, with an
  exit-status dot (verdigris 0 / amber non-zero / amber `◐` running) and an
  expandable stdout/stderr block. Distinct from tool calls: tools are `[ … ]`
  brackets, processes are `$ …`.
- **Durability rhythm** (`DurabilityMarker`). `snapshot_taken` / `resumed`
  render as a faint centered verdigris marker (`⌑ snapshotted · 1.2 GiB`),
  tying the conversation to the snapshot/sleep/resume lifecycle — the Engrams
  signature, previously invisible in the transcript.
- **Operator interrupt / stop** (see below). During an active run the
  harness-waiting line carries a `✕ stop` control.
- **Run summaries** (`RunSummary`). Each run closes with a faint receipt —
  `↳ read 1 · edited 1 · ran 1 · 18s` — tallied from its tool/exec blocks, so
  long sessions are scannable without reading every block (`interrupted` when
  stopped).
- **Context-aware waiting verb** (`contextVerb`). The harness-waiting line shows
  the *actual* in-flight action — `running cargo check…`, `reading
  snapshot_uffd.rs…`, `editing …` — derived from the open exec/tool, falling
  back to the generic gerund (thinking/recalling/working) between turns. Like
  this chat's "Shelling…".
- **Markdown rendering** (`mdToHtml`). Final assistant/system messages render as
  Markdown — headings, **bold**/*italic*, `inline code`, fenced code blocks,
  bullet/ordered lists, blockquotes, links, rules — styled in-system (mono for
  code, square bullets, hairline-boxed inline code, verdigris-ruled code blocks).
  User turns stay plain. During streaming you'd render plain text and only
  markdown-render once the message completes.

### Interrupt — stopping the harness mid-run

Cancel is feasible and the mechanism is the **Agent SDK's `query.interrupt()`**,
not a signal: it stops the in-flight turn but keeps the session alive (you can
keep prompting). In `claude -p` headless mode SIGINT kills the process and a
plain stdin write is queued until the turn ends, so the SDK interrupt is the
right path; abort propagates through `AbortController.signal` to the spawned
shell, HTTP stream, and sub-agents.

For Engrams (agentd drives the harness inside the microVM, coordinator exposes
`/prompt` `/exec` `/shell`): add **`POST /sessions/:id/interrupt`** → agentd
calls `query.interrupt()` → coordinator emits a new **`run_interrupted`** SSE
event → the transcript shows an interrupted marker; the session stays `active`.
The kit's `store.interrupt(id)` models this (clears the pending reply, appends a
system note + `run_completed{ok:false}` + `harness_idle`). Verify against your
pinned SDK version — some releases didn't honor the abort mid-query.

**Tweaks** (top toolbar): mark-as-status, Fleet view (strata/table), inline
boot loaders, harness-waiting verb.

## The trace loader — where it belongs

The engram-trace loader (`../assets/engram-loader.html`, and `EngramMark` in
`mode="loop"`) is reserved for moments that genuinely map to its meaning — a
memory trace forming / a thought composing. Used sparingly so it stays
meaningful rather than decorative. In the kit:

- **Harness-waiting (primary):** after you send a prompt, the looping mark + a
  gerund (`thinking…` / `recalling…` / `working…`, a tweak) renders in the
  transcript right where the assistant's reply will land — cf. this chat's
  "Shelling…". It unmounts the instant the harness speaks; the real message
  ink-settles in. See `HarnessWaiting` in `transcript.jsx`. *Not* used for
  in-flight tool calls — those keep their `[ tool · … ]` bracket vocabulary.
- **Ambient pulse:** the nav-spine mark strikes once per poll tick and loops
  while any session is booting/resuming (`markStatus` tweak).
- **Session boot/resume:** inline, replacing the status glyph while a session
  is `created`/`guest_ready` (`bootLoader` tweak).

Deliberately **not** used for: generic refreshes, settings saves, tab switches,
button spinners — those use plain text states. The rule: the trace is for
"a sandbox/agent is doing memory work," nothing more.

> **Production caveat:** resume is often sub-second — faster than the loader's
> draw. Gate the boot/resume loader behind a ~150ms delay so fast paths don't
> flash it, and make it interruptible (snap to resolved the instant the session
> is live) so it never makes a fast resume *feel* slow.

## Responsive

The kit is responsive down to phone widths (on-call operators check the fleet
from mobile). Two breakpoints in `dashboard.css`:

- **≤768px (tablet):** the wide instrument grids stop holding their columns, so
  host **strata stack** (sandbox cells over a full-width capacity bar) and both
  data tables — the Fleet table and the Storage **durability ledger** — flip
  from columns to **label/value records** (header row hidden; each row becomes a
  titled block with the label absolute-left and the value right-aligned, driven
  by `data-label` attributes on the cells).
- **≤600px (phone):** the **nav spine** wraps (wordmark + § chip on top, the
  four tabs on their own horizontally-scrollable row), the surface head stacks,
  the vital strip and fleet rollup wrap, **session rows go two-line** (glyph +
  id + age over image + status via grid-areas), and the storage rollups drop to
  2-up.

The transcript's gutter margin-notes already collapse inline ≤1024px (in
`colors_and_type.css`).
