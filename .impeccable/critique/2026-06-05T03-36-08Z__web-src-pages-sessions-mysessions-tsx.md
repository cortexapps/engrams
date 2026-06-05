---
target: the sessions page (My/All sessions list)
total_score: 25
p0_count: 0
p1_count: 3
timestamp: 2026-06-05T03-36-08Z
slug: web-src-pages-sessions-mysessions-tsx
---
## Design Health Score

| # | Heuristic | Score | Key Issue |
|---|-----------|-------|-----------|
| 1 | Visibility of System Status | 2 | Main table flashes the empty state during load; no error state on My/All sessions |
| 2 | Match System / Real World | 3 | Strong vocabulary; Hosts/Snapshots are fleet data on a personal "My sessions" page |
| 3 | User Control and Freedom | 2 | No search/filter/sort on a list that grows; fixed lifecycle ordering only |
| 4 | Consistency and Standards | 3 | Cohesive system, but list pages drop the loading/error handling siblings (Members/Storage) have |
| 5 | Error Prevention | 3 | Read-heavy surface; little to get wrong |
| 6 | Recognition Rather Than Recall | 3 | Status word shown in table badges; rail glyphs have no visible key |
| 7 | Flexibility and Efficiency | 2 | No keyboard shortcuts, no filter, no bulk actions for the power-user audience |
| 8 | Aesthetic and Minimalist Design | 3 | High craft; stat band adds non-actionable numbers above the real content |
| 9 | Error Recovery | 2 | A failed fetch renders as "No sessions yet" |
| 10 | Help and Documentation | 2 | No glyph legend or inline help anywhere on the page |
| **Total** | | **25/40** | **Acceptable — high craft undercut by missing states and metric selection** |

## Anti-Patterns Verdict

Does this look AI-generated? **No.** This is a committed, opinionated system — the Aston-racing logbook identity (celadon paper, lime-as-fill-only, Saira display, glyph+text status vocabulary) is specific and consistently applied. The `StatReadout` is a genuine gauge cluster (mono figures, Saira caps, hairline grid), not the banned hero-metric card row. Deterministic detector: **clean (0 findings)** across the sessions pages and shared components. Browser visualization unavailable this session (no automation tool; dev server also needs a backend) — findings are from code review plus the clean detector.

## Overall Impression

The hard part is done well: the lifecycle-grouped table with glyph+text status, mono IDs, and the persistent rail are exactly right for the developer audience. What lets it down is the *unglamorous* layer — loading, empty, and error states — plus a stat band that shows the wrong four numbers for this page. The biggest opportunity: make the list pages behave under live state the way the rail (and sibling pages) already do, and make the top-of-page metrics personal and actionable.

## What's Working

- **Lifecycle grouping + status glyphs.** ACTIVE / IDLE — RESUMABLE / ARCHIVED with per-group counts, color never the sole status carrier (glyph + badge text). On-brand and instantly scannable.
- **The persistent SessionsRail.** Sorted buckets, stable order under the 1s refetch, skeletons while pending, an explicit error line, glyph re-toning for the dark spine. It is the best-behaved surface in the section.
- **`StatReadout` as a component.** Gauge cluster, not cards; mono figure dominating a quiet Saira caption. Dodges the hero-metric cliché by construction.

## Priority Issues

- **[P1] The list pages flash the empty state during load and have no error state.** `MySessions`/`AllSessions` read only `data` from `useSessions`; while pending, `data` is undefined → `all = []` → the table renders "No sessions yet," then repaints with rows. A failed fetch renders the same misleading line. The rail already destructures `{ data, isPending, error }`, and sibling pages (Members, Storage) render loading + error. The list pages are the outlier.
  - **Fix:** thread `isPending`/`error` into both pages; show skeleton rows (matching the rail's skeletons) while pending and a `text-destructive` error block on failure.
  - **Command:** /impeccable harden

- **[P1] The stat band shows fleet data on a personal page.** On "My sessions," the four gauges are Active, Idle, **Hosts**, **Snapshots**. Hosts and total snapshots are operator/fleet numbers a developer can't act on here, and Active/Idle duplicate the table's own group-header counts directly below. Two sources for the same number, two numbers that belong in the Operator cockpit.
  - **Fix:** make the readout personal and non-duplicative (e.g. Active / Idle / Archived / total or last-activity), or drop it on My Sessions and keep capacity gauges in Operator/Fleet where they're actionable.
  - **Command:** /impeccable distill

- **[P1] The empty state is a flat muted line at the primary activation moment.** A new user's first screen is the heading, four zero-value gauges, and one gray sentence. The "New session" CTA lives only in the header, away from the empty space that should invite the first action.
  - **Fix:** a real teaching empty state — a status glyph or mark, one line of guidance, and an inline "New session" button co-located with the emptiness.
  - **Command:** /impeccable onboard

- **[P2] No way to narrow the list as it grows.** No search, no status filter, no sortable columns; ordering is fixed (lifecycle then recency). The rail caps at 10. A developer with dozens of sessions has no path to a specific one except scrolling.
  - **Fix:** a filter input (id / image / status) above the table; optionally sortable Age column.
  - **Command:** /impeccable craft

- **[P2] Row navigation isn't fully keyboard-reachable.** The `<TableRow>` navigates on click but has no role/tabindex/key handler; only the nested id link is focusable. Keyboard users reach a row through the id link alone, while the visual affordance is the whole row.
  - **Fix:** make the row a proper link target (wrap, or row-level keyboard handler + role), or make the affordance clearly the id link only.
  - **Command:** /impeccable harden

## Persona Red Flags

**Alex (power user):** No keyboard shortcut to start a session or focus the list. No filter/search; no sortable columns; no bulk actions. The rail's 10-item cap with only a footer "My sessions" link slows the jump to an older session.

**Sam (accessibility):** Table rows are click-to-navigate but not keyboard-focusable as a unit; only the id link is tabbable. Status is glyph + badge text (good — not color-only). The load-time flash to "No sessions yet" isn't announced, so a screen-reader user may hear an empty list that then silently fills.

**Developer (PRODUCT.md primary, ~90%):** Wants to scan their own in-flight work fast and densely. The fleet/snapshot gauges are noise on this page; the duplicated Active/Idle counts add nothing over the group headers. They expect the list to behave under live state — and on first paint it briefly lies about being empty.

## Minor Observations

- Page descriptions use em dashes ("Bounded units of agent work — launch, watch, resume.", "Every session across the fleet — owner-attributed.") — a house-style call, flagged for consistency.
- No visible legend for the status glyphs; their meaning lives in code comments. The table badges carry the word, but the rail (glyph-only) does not.
- Long image paths have no truncation/max-width on the Image cell; a long repo path can stretch the row.
- The ACTIVE group always renders even at zero ("ACTIVE · 0 / none"). Defensible (always answer "is anything live?"), noted only as a deliberate choice to keep.

## Questions to Consider

- What are the four numbers a developer actually wants at the top of *their* sessions page, if any?
- Should opening a session be reachable without the mouse, given the keyboard-first audience?
- What does this page look like the moment a fetch fails mid-stream, not just on first load?
