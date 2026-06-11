# ADR 0040: Web UI migration — shadcn shell, TanStack Router, assistant-ui transcript

Status: 2026-06-07 — **Accepted.** Implemented across the
`assistant-ui-transcript` branch (rebased onto `main`) and validated locally:

- `pnpm build` (`tsc -b` + `vite build`) clean; `dist/ghostty-vt.wasm`
  emitted by the retained ghostty WASM plugin.
- `pnpm test` — 51 vitest tests green (10 files), including the
  `buildMessages` transcript-mapping unit suite.
- `pnpm lint` (`oxlint`) gate added to the CI `web:` job; husky pre-commit
  runs `oxfmt` via `lint-staged`.

The change is four independent migrations applied in one branch:

- **Dual sidebar shell**: a primary destinations rail (Sessions / Fleet /
  Storage / Settings) plus a per-section second sidebar, replacing the flat
  top `NavSpine` masthead.
- **TanStack Router**: code-based routes with `beforeLoad` admin guards,
  replacing `react-router-dom` and the `RequireAdmin` render wrapper.
- **shadcn/ui zinc shell**: every surface rebuilt on shadcn primitives; the
  ~1,500-line hand-written `theme.css` "Lab Notebook" stylesheet retired.
- **assistant-ui transcript**: `SessionDetail` migrated to
  `@assistant-ui/react`; the hand-rolled SSE-to-node renderer replaced by a
  pure, unit-tested `buildMessages` mapper.

## Context

The `web/` SPA had accumulated three distinct problems.

**Routing and IA.** `react-router-dom` v7 with a single-level route tree and a
top `NavSpine` masthead. Nested settings (profile / tokens / members / images /
registries) rendered as tabs inside one Settings page; My/All session scope was
local state. The structure made breadcrumb navigation impossible and forced
admin gating into a `RequireAdmin` render guard rather than the router.

**Component library.** The UI was built on ~1,500 lines of hand-written CSS
(`theme.css`) — a "Lab Notebook" design with Newsreader serif headings, amber
accents, and a paper/ink palette — plus bespoke component primitives. Every new
surface had to be authored from scratch; there was no shared primitive layer.

**Transcript.** `Transcript.tsx` and `transcriptFmt.ts` were hand-rolled
renderers that mapped SSE events directly to React nodes. As the event schema
grew (tool calls, shell steps, pull-request cards, file shares, checkpoints),
maintaining the renderer became load-bearing work coupled to the transport.

## Decision

### 1. TanStack Router (code-based, `beforeLoad` guards)

Replace `react-router-dom` with `@tanstack/react-router` using code-based route
definitions (`createRootRoute` / `createRoute`). Admin gating moves to
idiomatic `beforeLoad` guards that `throw redirect(...)`; the `RequireAdmin`
wrapper is deleted (the coordinator's `require_admin` layer remains the real
gate). Nested layout routes model the IA:

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
zinc + oklch tokens with a light/dark toggle via a class-based `ThemeProvider`.
Typography: Saira (display) + JetBrains Mono (code / IDs); Lucide icons (already
inside the shadcn-generated primitives).

**Shell changes:**
- `NavSpine` (top masthead) → `MainSidebar` (`app-sidebar.tsx`) — a collapsible
  icon rail; footer carries the user `DropdownMenu` + theme toggle.
- Each section's layout route owns its own (independently collapsible) section
  sidebar.
- Content header: `SidebarTrigger` (mobile) + breadcrumb.

**Surfaces rebuilt** on shadcn table / card / badge / dialog / form /
dropdown-menu / alert-dialog / progress:
- Sessions list (`@tanstack/react-table` grouped by lifecycle: ACTIVE /
  IDLE–RESUMABLE / ARCHIVED; status → `Badge`; scope → nested routes),
  New-session dialog (`Dialog` + `Form`).
- Fleet (host cards, capacity progress incl. the disk/mem/cpu utilization
  telemetry, drain confirm).
- Storage (rollup cards, durability ledger table).
- Settings sub-panels: Profile, Tokens, Members, Images, Registries — the
  bespoke `_form.tsx` pattern replaced by `react-hook-form` + `zod` + a thin
  shadcn `<Field>`.

**`theme.css` fully retired.** The Lab Notebook stylesheet and its serif/paper
identity are deleted; the zinc shell drives the whole app via Tailwind tokens
(`bg-background` et al.). SessionDetail no longer renders as an "old-paper
island" — the transcript migration below completes its move onto shadcn.

### 3. assistant-ui transcript

Migrate `SessionDetail` / `Transcript` to `@assistant-ui/react`. The prior
hand-written `Transcript.tsx` + `transcriptFmt.ts` SSE-to-node renderer is
replaced by:

- **`buildMessages.ts`** — a pure function mapping the coordinator's indexed
  SSE events to `@assistant-ui/react` message objects. Isolated and
  unit-tested.
- **`SessionThread`** — hosts the `@assistant-ui/react` `Thread` primitive
  wired to a local external-store runtime seeded from the session events.
- **`ShellToolPart`** — renders shell-step tool calls (terminal output) inline
  in the thread.
- **`SystemMessage`** — renders harness-idle / recovered-from-checkpoint
  receipts as non-message thread items.
- **Tool groups**: consecutive tool calls are coalesced into a collapsible
  `ToolGroup` (previously each call rendered independently).

### 4. Linting gate

- `oxlint` (static analysis) — `pnpm lint` runs `oxlint src`.
- `oxfmt` (formatter) for `*.{ts,tsx}` — quote normalisation applied across
  `src/`.
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
| `Transcript.tsx`, `transcriptFmt.ts` | `SessionThread`, `buildMessages.ts` |
| `theme.css` (~1,500 lines) | shadcn zinc tokens + Tailwind |
| `react-router-dom` | `@tanstack/react-router` |
| `@fontsource-variable/newsreader` | `@fontsource-variable/saira` |

## Added dependencies

Runtime: `@assistant-ui/react`, `@assistant-ui/react-markdown`,
`@tanstack/react-router`, `@tanstack/react-table`, `@hookform/resolvers`,
`class-variance-authority`, `clsx`, `cmdk`, `lucide-react`, `next-themes`,
`radix-ui`, `react-hook-form`, `react-hotkeys-hook`, `sonner`, `tailwind-merge`,
`tw-animate-css`, `tw-shimmer`, `zod`, `zustand`, `@fontsource-variable/saira`.

Dev: `husky`, `lint-staged`, `oxfmt`, `oxlint`.

Removed: `react-router-dom`, `@fontsource-variable/newsreader`.

## Out of scope / follow-ups

- **Mobile responsive**: the primary `Sidebar` uses shadcn's native off-canvas
  behaviour; section sidebars collapse to a horizontal nav strip below `md`. No
  further responsive work was scoped here.
- **PromptComposer / TerminalPane**: kept as-is; the assistant-ui migration
  covered transcript display only, not the live compose / shell surfaces.
- **Bundle size**: the main chunk is >500 kB minified; route-level
  code-splitting and `manualChunks` are a follow-up.
