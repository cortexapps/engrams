---
target: sessions sidebar (SessionsRail)
total_score: 28
p0_count: 0
p1_count: 1
timestamp: 2026-06-04T23-03-51Z
slug: web-src-pages-sessions-sessionsrail-tsx
---
# Critique — Sessions Rail (SessionsRail.tsx)

## Design Health Score

| # | Heuristic | Score | Key Issue |
|---|-----------|-------|-----------|
| 1 | Visibility of System Status | 3 | Skeletons, live glyphs, active highlight, relative age present; total count only implied |
| 2 | Match System / Real World | 2 | "See all" is vocabulary used nowhere else; reads as a near-synonym of "All sessions" |
| 3 | User Control & Freedom | 3 | Always one click back to the full lists; no traps |
| 4 | Consistency & Standards | 2 | Two footer rows, one with an icon and one without; "See all" != the "My sessions" label used in the mobile strip + old rail |
| 5 | Error Prevention | 3 | Nav-only; creation goes through a validated dialog |
| 6 | Recognition Rather Than Recall | 3 | Persistent visible list + highlight; dinged by the ambiguous footer pair |
| 7 | Flexibility & Efficiency | 3 | The rail is the efficiency win; no keyboard quick-switch yet |
| 8 | Aesthetic & Minimalist | 3 | Clean, but the only create action is grey-inert and the footer hierarchy is flat |
| 9 | Error Recovery | 3 | Plain-language error; self-heals via the 1s refetch |
| 10 | Help & Documentation | 3 | Labels self-documenting; no help needed for a rail |
| **Total** | | **28/40** | Good — the drag is the mine/all vocabulary (#2, #4) |

## Anti-Patterns Verdict
- **LLM assessment:** Not AI slop. Reuses real primitives (StatusGlyph, lifecycle sort, sidebar tokens). No gradient text, eyebrow, or side-stripes.
- **Deterministic scan:** detect.mjs -> [], exit 0. Clean, no false positives.
- **Visual overlays:** none — harness has no browser automation. Findings from source review.

## Overall Impression
Structure is right (live list, running-first, persistent highlight). The footer undercuts it: the "which sessions am I looking at?" question goes fuzzy. Biggest win: speak the app's existing vocabulary (My sessions / All sessions), each with the icon it already owns.

## What's Working
- Lifecycle reuse: same lifecycleOf + ORDER as the table; running surfaces identically in both.
- Selection legibility: open session highlighted via standard data-[active=true]; answers "where am I".
- Honest data voice: mono short-id primary, muted image second line, tabular age.

## Priority Issues

- **[P1] Vocabulary collision — "See all" vs "All sessions"** (#2, #4)
  Why: A first-timer can't tell if "See all" means all sessions in the system or all of mine; it sits above "All sessions" (fleet-wide) so they read as duplicates. "See all" is used nowhere else — mobile strip + original rail said "My sessions."
  Fix: Relabel footer link to "My sessions" (-> /sessions); relabel group header "Sessions" -> "Recent" so the list's identity (my recent) is distinct from the destination's (my full list). Keep "All sessions" -> /sessions/all.

- **[P2] Missing icon on the "See all" row** (#4)
  Why: Footer's two rows are mismatched — one carries ListChecks, the other nothing — breaking row rhythm/scannability.
  Fix: Give the My-sessions row the Layers icon (its glyph in the spine + mobile strip). Both footer rows become icon + label.

- **[P2] The create action is rendered inert** (#1, #8)
  Why: "New session" is the rail's only verb and the product's signature move, but it's a grey secondary button. PRODUCT.md reserves lime for primary buttons.
  Fix: Primary (lime) fill, full-width. Active rows use sidebar-accent (green), not lime, so no collision.

## Persona Red Flags
- Jordan (First-Timer): Stalls at the footer — "See all" and "All sessions" look identical; picks at random. Core complaint, confirmed.
- Alex (Power User): No keyboard quick-switch (must click rows); acceptable, running-first sort softens it.
- Sam (A11y): Status is glyph-shape + aria-label, not color-alone (good). Grey secondary "New session" has weaker affordance than a lime fill; the color fix helps.

## Minor Observations
- "Recent" header makes the 10-item cap honest; pair with a total-count badge on My-sessions so overflow is visible.
- Empty state is bare text, but New session sits right above it.
