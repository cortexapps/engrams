---
target: web/src (engrams app — IA + typography + Aston conformance)
total_score: 28
p0_count: 0
p1_count: 2
timestamp: 2026-06-04T20-09-34Z
slug: eb-src-engrams-app-ia-typography-aston-conformance
---
# Critique — engrams web app: IA, design & Aston/racing conformance

Target: web/src (app-wide). Focus: information architecture, typographic system, conformance to the "Mont Blanc logbook / Aston-racing" identity. Browser automation unavailable — visual assessment grounded in source + PRODUCT.md intent, no live overlay.

## Design Health Score

| # | Heuristic | Score | Key Issue |
|---|-----------|-------|-----------|
| 1 | Visibility of System Status | 3 | Strong status glyphs + durability rail + run receipts; nav gives no "which session" context |
| 2 | Match System / Real World | 3 | Domain language right for dev audience |
| 3 | User Control and Freedom | 3 | Composer stop/cancel, fork hints; no command palette |
| 4 | Consistency and Standards | 2 | Three competing label voices; SessionDetail diverges from PageHeading |
| 5 | Error Prevention | 3 | alert-dialog on destructive, zod + react-hook-form |
| 6 | Recognition Rather Than Recall | 3 | Labeled icon+text nav, labeled tabs |
| 7 | Flexibility and Efficiency | 3 | Cmd+Enter send, Cmd+B sidebar; no palette/bulk |
| 8 | Aesthetic and Minimalist Design | 3 | Calm/committed; noisy all-caps-tracked-mono texture |
| 9 | Error Recovery | 3 | Composer banners name state AND next move |
| 10 | Help and Documentation | 2 | Page descriptions + inline hints only |
| Total | | 28/40 | Good — weak axis is typographic consistency |

## Anti-Patterns Verdict

Not AI slop — the opposite. Committed, specific identity (celadon paper, bottle-green spine, carbon twill, lime-as-fill). Failure mode is inverse of slop: over-applied texture (uppercased tracked mono as universal label voice) next to under-used identity (Saira barely appears).

Detector: 1 finding, broken-image SystemMessage.tsx:153 — FALSE POSITIVE (dynamic src wrapped to next line). No overlay (no browser automation).

## Priority Issues

### [P1] Mono overloaded; Saira (identity voice) nearly absent
Saira appears in only 4 places (page titles, button caps, terminal italics, empty state). All instrument labels are uppercased JetBrains Mono: tab labels, table headers, stat-readout labels, SessionDetail eyebrows, Storage section heading. Result: (a) in StatReadout the figure and its label are the same family — no figure/label contrast; (b) Saira's racing width-axis never reaches the label layer, so Aston identity is carried by color alone. Fix: one label primitive = font-display + tracked caps for all section labels/table headers/tabs/stat labels/sidebar group labels; reserve mono for machine strings only (IDs, digests, timestamps, numbers, code). Verify Saira tracked caps at 0.65rem stay legible both themes. Command: /impeccable typeset

### [P1-IA] Inside a session, nav can't tell you which session
SessionDetail.tsx:54 claims the nav spine carries a sub-crumb (ADR 0029), but app-sidebar.tsx has no sub-crumb and the Breadcrumb primitive is never imported. No back affordance either. Only "where am I" cue is the mono ID in content. Fix: implement the sub-crumb or add a breadcrumb to the masthead using the existing primitive; fix/delete the stale comment. Command: /impeccable shape

### [P2] SessionDetail opts out of the masthead system
Every other page uses PageHeading (Saira title + lime index-tab on rule). SessionDetail hand-rolls a font-mono title + mono eyebrow, reproducing the lime bar by hand. Fix: extend PageHeading with a mono/as prop for the title so the ID stays mono but eyebrow/rule/spacing are inherited. Command: /impeccable layout

### [P2] Dead typographic infrastructure
@fontsource/ibm-plex-serif is a dependency but never imported/used. --font-serif (Georgia) is defined but no font-serif class anywhere. Breadcrumb primitive defined but never used. Fix: commit to a serif role or drop it; use/remove Breadcrumb. Command: /impeccable distill

### [P3] Third label-voice variant
Some labels are sans uppercase (SystemMessage, ProfilePanel, RegistriesPanel badge); second-rail group labels are sans Title-case (not caps). The P1 label primitive should absorb all variants.

## Persona Red Flags
- Alex (power user, ~90%): no command palette; no keyboard jump-back-with-context from a session; no bulk actions on sessions table.
- Sam (a11y): verify smallest Saira caps contrast on celadon light mode; rewound message announces only via title attr — verify SR.
- Priya (dev checking background agent): can't tell which of several sessions she's in from nav; no at-a-glance running/finished per session in list context.

## Minor Observations
- Rewound message border-l-2 muted left rule brushes the side-stripe ban (borderline-OK as de-emphasis).
- StatReadout near hero-metric cliché, saved by gauge-cluster framing.
- Fleet/Storage full-bleed vs Sessions/Settings second rail — unsignaled layout jump.

## Questions to Consider
- If Saira carried every label, would mono finally read as data and the stat clusters look like gauges?
- What's the confident version of "I'm in session a1b2c3"?
- Should the serif slot become a real voice or be deleted?
