---
target: SessionDetail page
total_score: 27
p0_count: 0
p1_count: 3
timestamp: 2026-06-04T17-58-12Z
slug: web-src-pages-sessiondetail-tsx
---
# SessionDetail critique

## Design Health Score

| # | Heuristic | Score | Key Issue |
|---|-----------|-------|-----------|
| 1 | Visibility of System Status | 3 | Status glyph + live event count good; heartbeat animation is dead (class undefined) |
| 2 | Match System / Real World | 3 | Domain vocabulary (session/image/COW/rpo) is right for the audience |
| 3 | User Control and Freedom | 3 | Tabs + no-destructive surface; intentional no-back per ADR 0029 |
| 4 | Consistency and Standards | 2 | Two mastheads, two table styles, two status vocabularies, dead tokens vs migrated list |
| 5 | Error Prevention | 3 | Read-mostly surface; little to get wrong |
| 6 | Recognition Rather Than Recall | 3 | Labeled tabs, visible ids |
| 7 | Flexibility and Efficiency | 2 | No keyboard tab-switch / shortcuts for a power-user audience |
| 8 | Aesthetic and Minimalist | 3 | Clean, but vertical metadata stack pushes the transcript below the fold |
| 9 | Error Recovery | 3 | CowState conditional copy is genuinely honest; shell-close message is helpful |
| 10 | Help and Documentation | 2 | None; acceptable for a dev tool |
| **Total** | | **27/40** | **Acceptable — significant consistency work before it feels finished** |

## Anti-Patterns Verdict

Detector scan of `SessionDetail.tsx`: clean (`[]`). Not AI-slop. The real problem is the opposite of generic: it is a pre-migration island. The page rides on **undefined legacy custom properties** (`--color-ink`, `--color-ink-faded`, `--color-ink-quiet`, `--color-amber`, `--color-rule`, `--color-verdigris`, `--color-paper`) and dead classes (`.smallcaps`, `.glyph-heartbeat`, `.terminal-host`). They resolve to nothing, so text inherits `currentColor` and the active-session heartbeat / amber tab underline silently no-op. The terminal renders cream-solarized inside an otherwise celadon/racing app.

## Priority Issues

### [P1] Should the header metadata move to a right rail? — Yes, mostly.
The header stacks five strata (eyebrow, id, status, image/age/events, COW table) above the tabs, so the transcript — the actual work — starts well below the fold. That metadata is reference material ("the receipts"), not the content. Moving it to a right rail is the canonical object-detail pattern (Linear issue panel, GitHub PR sidebar) the audience already trusts, keeps durability/status persistently visible across tab switches, and reclaims full viewport height for the transcript. Keep the session id as a top masthead; move only the instrument metadata. Make the rail collapsible / hidden on the SHELL tab (the terminal wants width), and collapse to a top strip on mobile.

### [P1] Dead design tokens across the SessionDetail surface
`SessionDetail`, `CowState`, `TabRow`, `Glyph`, `TerminalPane` all reference removed tokens/classes. Migrate to shadcn/aston tokens (`text-foreground`, `text-muted-foreground`, `border-border`, `bg-primary`) matching the already-migrated sessions list. Re-theme the terminal palette to racing + theme-aware (it is hardcoded light solarized today).

### [P1] Two mastheads — consistency break
Every other page uses `PageHeading` (Saira display title, lime index-tab bar, hairline rule). SessionDetail hand-rolls a mono eyebrow + h1 with inline styles. Adopt `PageHeading` (id in the title slot, status/actions in the actions slot).

### [P2] Two status vocabularies
The migrated list uses a `Badge variant={statusVariant}`. The detail uses a glyph + lowercase text. Standardize so status reads identically on both surfaces.

### [P2] Two table vocabularies
The list uses the shadcn `Table`. The COW readout is an inline CSS-grid with dotted borders. A distinct instrument readout is defensible, but it should at least use `border-border` / `muted` tokens, not dead ones.

## Persona Red Flags

**Alex (power user):** no keyboard tab-switching (1/2/3 or ⌘-arrows), no shortcuts; tabs are click-only.
**Sam (a11y):** status heartbeat is dead so "active" relies on the glyph alone; the dead tokens mean contrast is whatever it happens to inherit, not a verified ≥4.5:1; terminal cursor amber is off-palette.

## Minor Observations
- The RAW tab is a dense inline-styled grid; functional, low priority.
- `relativeTime` is imported from the old `SessionManifest` while the migrated list imports its own copy from `session-format.ts` — two implementations of the same helper.
