# Design: Migrate engrams-web to pure shadcn/ui

**Date:** 2026-06-03
**Branch:** `shadcn-setup`
**Status:** Shaped & confirmed via brainstorming. Ready for implementation plan.

## Summary

Rebuild the `web/` frontend on **pure shadcn/ui**: stock shadcn components, the
default **zinc** theme with a **dark-mode toggle**, and shadcn's default sans
typography. The current bespoke "Lab Notebook" design (paper/amber palette,
Newsreader serif, ~1,500 lines of hand-written CSS in `theme.css`) is **set
aside** for now.

This **reverses** the prior locked brief (2026-06-02), which had said to adopt
shadcn primitives but *discard* the default theme and keep the Lab Notebook
tokens. The new direction is the opposite: pure shadcn, default theme.

The IA changes from a top-navbar masthead to a **dual sidebar**: the four
top-level destinations become the primary left **sidebar**, and each section's
sub-navigation becomes a **second sidebar owned by that section's layout route**.

**The transcript / SessionDetail surface is explicitly OUT OF SCOPE** — it
migrates to assistant-ui later and is left untouched here.

## Decisions (locked during brainstorming)

1. **Migration depth:** Full rebuild using shadcn idioms (not a CSS reskin).
2. **Theme baseline:** zinc + light/dark toggle.
3. **Scope:** all in-scope surfaces in one plan (Sessions, Fleet, Storage,
   Settings + its panels). SessionDetail excluded.
4. **IA:** top navbar → primary sidebar; secondary navs (My/All Sessions;
   Settings sub-panels) → a second sidebar, owned per-section.
5. **Routing:** keep the existing **code-based** TanStack Router; add nested
   **layout routes** for `/sessions` and `/settings` that render the section
   sidebar + their own `<Outlet/>`. No switch to file-based.
6. **Typography:** pure shadcn default sans. Drop Newsreader. Keep JetBrains
   Mono mapped to `--font-mono` for IDs/code/telemetry only.
7. **Icons:** Lucide (shadcn default) everywhere. No Phosphor — lucide already
   ships inside the generated primitives, so one library, no double-bundle.
8. **Mobile responsive.** The primary `Sidebar` uses shadcn's native off-canvas
   mobile behaviour. The section (second) sidebars are NOT collapsible natively,
   so they show only at `md+` and collapse to a horizontal scrollable nav strip
   below `md`. Tables rely on shadcn's `overflow-x-auto` wrapper; stat grids
   reflow via responsive `grid-cols`. Verified at 375px width.

## Current state (for reference)

- **Stack:** Vite + React 19 + Tailwind v4 + TanStack Router (code-based) +
  React Query. No shadcn installed yet despite the `shadcn-setup` branch name.
- **Shell:** top sticky `NavSpine` masthead with four flat tabs.
- **Surfaces:** Sessions (`/`), SessionDetail (`/sessions/$id`, excluded),
  Fleet (`/fleet`, admin), Storage (`/storage`, admin), Settings (`/settings`
  with sub-panels profile/tokens/members/images/registries).
- **Theme:** `web/src/theme.css`, ~1,500 lines of bespoke CSS.

### Current color scheme (recorded before discarding)

- Backgrounds: paper `#f4eedf`, paper-warm `#efe7d3`
- Text: ink `#1b1612`, faded `#5c544a`, quiet `#8a8275`
- Hairlines: rule `#d9cfb8`, rule-faint `#e7decb`
- Accents: amber `#b85c0a` ("now"/live/primary), verdigris `#3a6b5c`
  ("archived"/snapshotted)
- Fonts: Newsreader (serif display) + JetBrains Mono; square corners
  (`--radius: 0`); light-only.

## Architecture

### Foundation

- Init shadcn (`components.json`, `cn()` util in `src/lib/utils.ts`, `@/` path
  alias in `tsconfig` + `vite.config`).
- Add deps: `class-variance-authority`, `clsx`, `tailwind-merge`,
  `lucide-react`, `tw-animate-css`, `@tanstack/react-table`. Radix primitives
  are pulled per-component by the CLI.
- New `src/index.css`: shadcn zinc tokens in oklch (`:root` + `.dark`),
  `@theme inline` mapping, default radius, `@import "tailwindcss"` and the
  animate import. `--font-mono` → JetBrains Mono.
- **Dark mode:** a small class-based `ThemeProvider` (`src/components/theme-provider.tsx`)
  toggling `.dark` on `<html>`, persisting to `localStorage`. No `next-themes`.
- Pull components: `sidebar`, `button`, `card`, `table`, `badge`, `tabs`,
  `dialog`, `dropdown-menu`, `select`, `input`, `label`, `form`, `progress`,
  `tooltip`, `separator`, `skeleton`, `sonner`, `alert-dialog`, `avatar`.

### Shell + routing

Nested layout routes (code-based, in `router.tsx`):

```
__root (providers, Outlet)
└─ RootLayout         <SidebarProvider> MainSidebar | <header + Outlet>
   ├─ '/'             → redirect to /sessions
   ├─ /sessions       SessionsLayout: section sidebar | <Outlet>
   │    ├─ /sessions/        → MySessions
   │    ├─ /sessions/all     → AllSessions (admin, owner-attributed)
   │    └─ /sessions/$id     → SessionDetail  (EXCLUDED — untouched)
   ├─ /settings       SettingsLayout: section sidebar | <Outlet>
   │    └─ profile · tokens · members · images · registries
   ├─ /fleet          → full-bleed (no second sidebar)
   └─ /storage        → full-bleed (no second sidebar)
```

- **MainSidebar** — shadcn `Sidebar` (collapsible icon rail). Header: EngramMark
  + "engrams" wordmark. Nav: Sessions / Fleet / Storage / Settings with lucide
  icons, admin-gated (Fleet/Storage admin-only), active state via
  `useRouterState`. Footer: user `DropdownMenu` (replaces `UserChip`) + theme
  toggle. Replaces `NavSpine`.
- **Section sidebars** — each owned by its section's layout route. Built from
  `SidebarMenu` primitives inside a scoped `SidebarProvider` so each second
  sidebar is independently collapsible and is a real sidebar (not page
  furniture). Sessions: My sessions / All sessions (admin). Settings: grouped
  "You" (Profile, Tokens) and "Deployment" (Members, Images, Registries,
  admin-only).
- **Content header** — slim bar with `SidebarTrigger` (mobile) + breadcrumb.
  The main `<Outlet>` content area is full height/width.
- **Admin gating** — keep the existing `requireAdmin` `beforeLoad` guards. The
  coordinator's `require_admin` remains the real gate; sidebar hiding is UX-only.

### Surface → component mapping

| Surface | Current (bespoke) | shadcn rebuild |
|---|---|---|
| Sessions list | `.session-row` grid + `ManifestGroup` | `@tanstack/react-table` + `Table`, grouped by lifecycle (ACTIVE / IDLE–RESUMABLE / ARCHIVED) via section header rows; status → `Badge`; age mono; owner (All view) → `Avatar` + email |
| Scope (My/All) | in-page `ScopeTab` `useState` | second-sidebar items → routes `/sessions` and `/sessions/all` |
| New session | inline animated form | `Button` → `Dialog` with `Form` + `Input` + `Select` |
| Vital strip | `.vital-strip` | small `Card` stat row |
| Token nudge | `.token-nudge` | `Alert`-style `Card` with action `Button` linking to `/settings/tokens` |
| Fleet | host `.stratum` + `.cap-bar` | host `Card`s; capacity → `Progress`; status → `Badge`; drain → `Button` + `AlertDialog` confirm; sandbox cells kept as small squares; rollup `Card`s; reconciler note inline |
| Storage | rollups + `.ledger` | rollup `Card`s; durability ledger → `Table`; base locality → `Progress`; RPO mono (hot state via `text-` token, not amber) |
| Settings shell | `.settings-groups` tabs + Outlet | layout route → second sidebar (You / Deployment, admin-gated); panels rebuilt |
| Members | `.members-ledger` grid | `Table`; role → `Badge`; role/active actions → `DropdownMenu` + `AlertDialog`; provenance as muted text |
| Profile / Tokens / Images / Registries | bespoke forms (`_form.tsx`) | `Form` + `Input` + `Label` + `Button` + `Card`; lists → `Table` |

### Excluded / kept (interim coexistence)

- **SessionDetail + all transcript components** untouched: `Transcript`,
  `transcriptFmt`, `ToolCall`, `Process`, `RunSummary`, `RunBoundary`,
  `UserTurn`, `Markdown`, `HarnessWaiting`, `PromptComposer`, `TerminalPane`,
  `ArtifactCard`, `PullRequestCard`, `DurabilityMarker`, `CowState`,
  `VitalSigns`, `Glyph`.
- **`theme.css` is NOT deleted.** It stays imported so SessionDetail still
  renders. The new shell drives the app background via shadcn `bg-background`;
  **SessionDetail will look like an "old-paper island" inside the zinc app
  until the assistant-ui migration.** This is a known, accepted interim state.
- `framer-motion` stays (used by transcript). `EngramMark` SVG kept as the
  brand mark.
- **Removed once their surfaces are rebuilt:** `NavSpine`, `VitalStrip`,
  `ManifestGroup`, `SessionManifest`, the bespoke parts of `Identity.tsx`,
  `TabRow`, `SectionHead`, `UserChip`.

### Testing

- Update existing component tests to the new components: `NewSessionForm.test`,
  `ImagesPanel.test`, `RegistriesPanel.test`, `UserChip.test` (→ user menu).
- `test-utils.tsx` stays async (TanStack `RouterProvider` defers first paint).
- `TerminalPane.test` / `Transcript.test` untouched (excluded surfaces).
- Add light render tests for the primary sidebar, the two section sidebars, and
  the dark-mode toggle.

## Risks / notes

- **Old-paper island:** SessionDetail's visual mismatch is intentional and
  temporary. Do not attempt to restyle it here.
- **CSS coexistence:** shadcn tokens (`--background`, `--foreground`, …) and the
  legacy Lab Notebook tokens (`--color-paper`, `--fg`, …) use different names
  and coexist. The one conflict — the global `body` background set by
  `theme.css` — is resolved by letting the shadcn shell own the app background;
  the legacy body rule is removed/overridden while keeping the rest of
  `theme.css` for the transcript subtree.
- **Memory correction:** the prior `engram-web-redesign` memory's "discard
  default theme, keep Lab Notebook tokens" decision is now superseded.

## Out of scope

- SessionDetail / transcript redesign (→ assistant-ui, separate effort).
- Any new product features. This is a presentation-layer migration only.
