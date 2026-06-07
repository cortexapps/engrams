# ADR 0040: Web UI migration — shadcn shell, TanStack Router, assistant-ui transcript

Status: 2026-06-05 — **Accepted.** Implemented across this branch and
validated locally (`pnpm build` clean, `pnpm test` green, full app exercised in
the `just dev` loop):

- **Dual sidebar shell**: primary destinations rail (Sessions / Fleet /
  Storage / Settings) + per-section second sidebar (My/All sessions, settings
  sub-panels), replacing the flat top `NavSpine`.
- **Session list**: `@tanstack/react-table` grouped table with lifecycle
  sections (ACTIVE / IDLE–RESUMABLE / ARCHIVED); status → `Badge`; scope
  (`My` / `All`) → nested layout routes.
- **New-session dialog**: `Dialog` + `Form` (react-hook-form + zod), replacing
  the inline animated form.
- **Settings panels** rebuilt on shadcn: Profile, Tokens, Members, Images,
  Registries — bespoke `_form.tsx` deleted.
- **Session transcript**: migrated to `@assistant-ui/react`; tool calls
  coalesced into a collapsible `ToolGroup`; shell steps rendered inline.
- **Linting gate**: `oxlint` + `oxfmt` + husky pre-commit hook; `pnpm lint`
  added to the CI `web:` job.

## Context

The `web/` SPA had accumulated two distinct problems:

**Routing and IA.** `react-router-dom` v7 with a single-level route tree and a
top `NavSpine` masthead. Nested settings (profile/tokens/members/images/
registries) rendered as tabs inside a single Settings page; My/All session scope
was local state. The structure made breadcrumb navigation impossible and forced
admin-gate logic into render guards (`RequireAdmin` component) rather than the
router.

**Component library.** The UI was built on ~1,500 lines of hand-written CSS
(`theme.css`) — a "Lab Notebook" design with Newsreader serif headings, amber
accents, paper/ink palette, and bespoke component primitives. Every new surface
required authoring from scratch; there was no shared shadcn primitive layer.

**Transcript.** `Transcript.tsx` and `transcriptFmt.ts` were hand-rolled
renderers that directly mapped SSE events to React nodes. As the event schema
grew (tool calls, shell steps, pull-request cards, file shares, checkpoints),
maintaining the renderer became load-bearing work disconnected from the
transport layer.

## Decision

Four independent migrations, applied in one branch:

### 1. TanStack Router (code-based, beforeLoad guards)

Replace `react-router-dom` with `@tanstack/react-router` using code-based route
definitions (`createRootRoute` / `createRoute`). Admin gating moves to idiomatic
`beforeLoad` guards that `throw redirect(...)` — the `RequireAdmin` wrapper
component is deleted; the coordinator's `require_admin` layer remains the real
gate. Nested layout routes model the IA:

```
__root (providers, Outlet)
└─ RootLayout         <SidebarProvider> MainSidebar | <header + Outlet>
   ├─ /               → redirect to /sessions
   ├─ /sessions       SessionsLayout: section sidebar | <Outlet>
   │    ├─ /sessions/      → MySessions
   │    ├─ /sessions/all   → AllSessions (admin)
   │    └─ /sessions/$id   → SessionDetail
   ├─ /settings       SettingsLayout: section sidebar | <Outlet>
   │    └─ profile · tokens · members · images · registries
   ├─ /fleet          → full-bleed
   └─ /storage        → full-bleed
```

### 2. shadcn/ui zinc shell

Initialise shadcn (`components.json`, `cn()` util, `@/` path alias). Theme:
zinc + oklch tokens; light/dark toggle via a class-based `ThemeProvider` (no
`next-themes`). Typography: Saira (display) + JetBrains Mono (code / IDs).
Icons: Lucide (already inside shadcn-generated primitives — no Phosphor
duplication).

**Shell changes:**
- `NavSpine` (top masthead) → `MainSidebar` — collapsible icon rail; footer:
  user `DropdownMenu` + theme toggle.
- Section sidebars owned by each section's layout route (independently
  collapsible).
- Content header: `SidebarTrigger` (mobile) + breadcrumb.

**Surfaces rebuilt** on shadcn table / card / badge / dialog / form /
dropdown-menu / alert-dialog / progress:
- Sessions list, New session dialog
- Fleet (host cards, capacity progress, drain confirm)
- Storage (rollup cards, durability ledger table)
- Settings sub-panels

**Forms**: `react-hook-form` + `zod` + a thin shadcn `<Field>` primitive
replacing the bespoke `_form.tsx` pattern.

**Kept as-is (interim coexistence)**: `theme.css` is not deleted; its
SessionDetail subtree styles remain valid. The new shell drives the app
background via `bg-background`; SessionDetail renders as an "old-paper island"
until the separate transcript migration (done in the same branch — see below).

### 3. assistant-ui transcript

Migrate `SessionDetail` / `Transcript` to `@assistant-ui/react`. The prior
hand-written `Transcript.tsx` + `transcriptFmt.ts` SSE-to-node renderer is
replaced by:

- **`buildMessages.ts`** — a pure function that maps the coordinator's indexed
  SSE events to `@assistant-ui/react` message objects. Isolated and unit-tested.
- **`SessionThread.tsx`** — hosts the `@assistant-ui/react` `Thread` primitive
  wired to a local external-store runtime seeded from the session events.
- **`ShellToolPart.tsx`** — renders shell-step tool calls (terminal output,
  inline) inline inside the thread.
- **`SystemMessage.tsx`** — renders harness-idle / recovered-from-checkpoint
  receipts as non-message thread items.
- **Tool groups**: consecutive tool calls are coalesced into a collapsible
  `ToolGroup` (expand/collapse chevron). Previously each call rendered
  independently.

### 4. Linting gate

- `oxlint` (Rust-based static analysis) added to the dev-dependency set.
- `oxfmt` (Rust-based formatter) for `*.{ts,tsx}` — enforces double-quote
  style (quote normalisation applied across `src/`).
- `husky` pre-commit hook runs `oxfmt` via `lint-staged`.
- CI `web:` job adds `pnpm lint` before `pnpm build`.

## Key deleted code

| Removed | Replaced by |
|---|---|
| `NavSpine.tsx` | `app-sidebar.tsx` (MainSidebar) |
| `RequireAdmin.tsx` | `beforeLoad` route guards |
| `ManifestGroup.tsx`, `SessionManifest.tsx` | `@tanstack/react-table` grouped rows |
| `TabRow.tsx`, `SectionHead.tsx`, `VitalStrip.tsx` | shadcn `Sidebar` / `Card` |
| `UserChip.tsx` | `user-menu.tsx` (`DropdownMenu`) |
| `NewSessionForm.tsx` | `NewSessionDialog.tsx` (shadcn `Dialog` + `Form`) |
| `_form.tsx` (settings) | `field.tsx` + react-hook-form |
| `Transcript.tsx`, `transcriptFmt.ts` | `SessionThread.tsx`, `buildMessages.ts` |
| `Process.tsx`, `RunBoundary.tsx`, `HarnessWaiting.tsx` | `session-thread/` components |
| `react-router-dom` | `@tanstack/react-router` |
| `@fontsource-variable/newsreader` | `@fontsource-variable/saira` |

## Added dependencies

Runtime: `@assistant-ui/react`, `@assistant-ui/react-markdown`,
`@tanstack/react-router`, `@tanstack/react-table`, `@hookform/resolvers`,
`class-variance-authority`, `clsx`, `tailwind-merge`, `lucide-react`,
`radix-ui`, `react-hook-form`, `react-hotkeys-hook`, `sonner`, `tw-animate-css`,
`tw-shimmer`, `zod`, `zustand`, `@fontsource-variable/saira`,
`cmdk` (command palette).

Dev: `husky`, `lint-staged`, `oxfmt`, `oxlint`.

Removed: `react-router-dom`, `@fontsource-variable/newsreader`.

## Out of scope / follow-ups

- **Mobile responsive**: the primary `Sidebar` uses shadcn's native off-canvas
  behaviour; section sidebars collapse to a horizontal nav strip below `md`. No
  further responsive work was scoped here.
- **SessionDetail old-paper island**: `theme.css` stays; the zinc shell and the
  Lab Notebook SessionDetail coexist via CSS namespace separation. Full removal
  of `theme.css` is a follow-up once SessionDetail is fully on shadcn.
- **PromptComposer / TerminalPane**: kept as-is; the assistant-ui migration
  covered transcript display only (not the live compose / terminal surfaces).
