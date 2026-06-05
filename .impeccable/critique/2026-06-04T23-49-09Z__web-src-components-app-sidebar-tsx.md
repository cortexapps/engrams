---
target: the main sidebar
total_score: 27
p0_count: 0
p1_count: 2
timestamp: 2026-06-04T23-49-09Z
slug: web-src-components-app-sidebar-tsx
---
# Critique: the main sidebar (app-sidebar.tsx + its IA)

## Design Health Score

| # | Heuristic | Score | Key Issue |
|---|-----------|-------|-----------|
| 1 | Visibility of System Status | 3 | Active states clear; rail shows no live operational signal (draining host, hot RPO) the operator hat needs |
| 2 | Match System / Real World | 3 | Sessions/Fleet/Storage are good nouns; Settings overloaded with org-level deployment config |
| 3 | User Control and Freedom | 3 | Standard nav, icon-collapse, admin-guard redirects |
| 4 | Consistency and Standards | 2 | Two entry points to Settings; top-level destinations inconsistently expand a second rail vs render full-bleed; operator concerns split across rail + Settings |
| 5 | Error Prevention | 3 | Admin guards + drain confirmation dialog |
| 6 | Recognition Rather Than Recall | 2 | Which Settings? + operator config hidden under Settings to Deployment, unreachable from Fleet/Storage |
| 7 | Flexibility and Efficiency | 2 | No keyboard accelerators / command palette for a keyboard-first audience |
| 8 | Aesthetic and Minimalist Design | 4 | Excellent: carbon-fade bottle-green spine, icon-collapse, restrained item count, on-brand |
| 9 | Error Recovery | 3 | n/a for navigation |
| 10 | Help and Documentation | 2 | Tooltips only when collapsed; no contextual hints or shortcut discovery |
| Total | | 27/40 | Acceptable. Surface is beautiful, IA is the problem |

## Anti-Patterns Verdict

Does this look AI-generated? No. Carbon-thread spine, bottle-green-against-celadon notebook cover, lime-fill-only accent discipline, section rail one luminance step lighter than the primary. A committed, specific identity.

Deterministic scan: detect.mjs --json over app-sidebar, RootLayout, SettingsLayout, SessionsLayout, user-menu returned []. Zero findings.

Visual overlays: not available this session (no browser-automation tool). Dev server live at :5173 for a manual/`live` pass.

Problem is IA, not craft.

## Overall Impression

The sidebar is the best-looking part of the app and one of the least-resolved structurally. PRODUCT.md says the user wears two hats: developer (90% of sessions) and operator. The rail models four flat sibling destinations where the operator's concerns are scattered across three of them.

Operator responsibilities today live in: Fleet (rail) hosts/capacity/drain; Storage (rail) chunks/snapshots/COW durability; Settings to Deployment sub-group Members/Images/Registries. All one job. Splitting hosts/capacity from the images and registries those hosts run, and filing org membership under the same gear as a personal profile, is the tab-soup failure the anti-references call out.

## What's Working

1. The aesthetic system is resolved and disciplined. Lime is a fill never light-mode text; the active marker is the only saturated thing on the dark spine; section rail derives from the primary by one luminance step.
2. Item count respects working memory: 2-4 destinations, non-admin sees a two-item rail.
3. Collapsed-state tooltips + text labels: never icon-only navigation.

## Priority Issues

### [P1] Operator surfaces are fragmented; there is no operator home
Why: one person, one job, spread across two rail destinations and one buried Settings group. No single instrument panel, exactly what PRODUCT.md asks for.
Fix: one rail destination, Operator (or Infrastructure), owning its own second rail: Overview (cockpit), Fleet, Storage, Images, Registries, Members. Overview is the landing: capacity gauges, draining-host count, aggregate durability/RPO, GC backlog, all drill-downs. Sessions = developer hat, Operator = admin hat.
Command: shape, then craft for the cockpit.

### [P1] Settings has two entry points to the same screen
Why: the rail gear (/settings -> /settings/profile) and the avatar "Settings" land on the identical Profile panel. For the 90% non-admin, the gear is pure redundancy. Two doors to one room is a recall tax and a consistency violation.
Fix: personal settings (Profile, Tokens) live under the avatar menu only; remove the gear from the rail. Org/deployment items move into Operator. Rail loses the gear; Settings stops meaning two scopes.
Command: shape.

### [P2] Top-level destinations behave inconsistently
Why: Sessions and Settings expand a second rail; Fleet and Storage render full-bleed with none. No way to predict which from the rail.
Fix: after consolidation, every top-level destination (Sessions, Operator) owns a second rail; personal config lives in the avatar. Consistency by structure.
Command: shape.

### [P2] No keyboard accelerators for a keyboard-first audience
Why: developers expect keyboard reach; no command palette, no go-to shortcuts. Cmd-K finds nothing.
Fix: Cmd-K command palette (shadcn command) for destination jumps + session switching, plus g-s / g-o bindings.
Command: craft after IA settles.

### [P3] The rail carries no live operational signal
Why: calm-under-live-state product, but the rail is inert. A draining host never surfaces until you navigate in.
Fix: a quiet glyph/count badge on Operator (claret only when attention needed), within the existing glyph+color vocabulary.
Command: delight or fold into the cockpit craft.

## Persona Red Flags

Alex (power user): no Cmd-K, no go-to. Reaching Registries is four mouse hops. Resents it.
Jordan (first-timer, non-admin): rail gear Settings + avatar Settings are the same screen; has to click both to learn that.
Operator Morgan (from PRODUCT.md): needs panels that read like instruments; today assembles fleet+images+storage health across three routes in their head. The promised cockpit does not exist.

## Minor Observations

- border-r-sidebar (line 33) sets a border color with no width utility; renders nothing, matching the intended no-divider primary rail. Works by accident; comment the intent.
- Item type redeclared in both SettingsLayout and SessionsLayout; extract a shared NavItem during the reshape.
- Layers icon used for both the Sessions destination and "My sessions" child; muddies the active read.

## Questions to Consider

- If Sessions and Operator are the two hats, does the rail need more than those two plus the avatar?
- Should the Operator landing be read-only (gauges + drill-downs) or actionable (drain/revoke from the overview)? Design the everything-green glance first.
- Is "Settings" the right word for anything once personal config is under the avatar and infra is under Operator?
