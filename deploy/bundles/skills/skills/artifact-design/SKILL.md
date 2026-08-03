---
name: artifact-design
description: Design guidance for authoring artifacts (the Artifact tool). Read BEFORE writing any HTML artifact — a report, dashboard, landing page, tool, or document the user will keep or share. Produces pages with deliberate palette, typography, and layout instead of templated AI-default designs, correct in both light and dark themes.
---

# Designing engrams artifacts

Approach this as the design lead at a small studio known for versatility,
giving every deliverable a visual identity pitched at the treatment the
task actually calls for. Make deliberate choices about palette,
typography, and layout that are specific to this subject, and avoid
templated designs.

## Markdown or HTML — pick the right medium first

Markdown artifacts are rendered by the engrams product in its own house
style: typographic hierarchy, code highlighting, light/dark — all free.
When the content is a document (notes, a report of findings, a plan),
publish markdown and spend zero effort on styling. Write HTML only when
the design IS part of the deliverable: a dashboard, an interactive tool,
a landing page, a visualization. The rest of this skill is about HTML.

## Read the request first

Calibrate treatment, not whether to design. A doc deserves the same
craft as a landing page — what changes is the treatment.

Many requests call for a utilitarian treatment: a plan, a memo, a demo.
Make it polished — real typographic hierarchy, considered spacing, a
proper palette — but avoid over-designing. Most pages do not need a
flashy hero. Some requests call for an editorial treatment: a landing
page, a game, a tool the user will keep or share. When unsure: a
well-composed page is never the wrong answer; an over-designed visual
identity sometimes is.

## The engrams contract

- **Fully self-contained, one file.** Inline all CSS and JS. No CDN
  links, no external fonts, no remote images — the page must render
  complete inside a sandboxed frame with nothing but its own bytes. For
  type, use a well-chosen system stack or an inlined @font-face data
  URI; never a webfont URL that can silently fall back.
- **Honor the viewer's theme.** engrams opens the page with
  `?theme=light` or `?theme=dark` in the URL. Define the palette as
  custom properties on `:root`, redefine only the tokens under
  `@media (prefers-color-scheme: dark)`, then apply the query parameter
  as the override in both directions:

  ```js
  const theme = new URLSearchParams(location.search).get("theme");
  if (theme) document.documentElement.dataset.theme = theme;
  ```

  with `:root[data-theme="dark"]` / `:root[data-theme="light"]` token
  blocks that win over the media query. Style components through the
  tokens, never directly inside the media query. Give the second theme
  the same care as the first — don't naively invert; keep contrast
  legible and the accent working on both grounds. A design that
  deliberately commits to one visual world may stay single-theme — make
  it a choice, not an omission.
- **Versions are cheap.** `Artifact update` publishes the next revision
  at the same stable URL, and viewers can flip between revisions — so
  iterate rather than hedge inside one page.

## Fundamentals for every artifact

**Ground it in the subject.** Pin one concrete subject, its audience,
and the page's single job. The subject's own world — its materials,
instruments, vernacular — is where distinctive choices come from. Build
with real content throughout, never lorem.

**Pair typefaces.** Typography carries the page even when the page
isn't about typography. Keep running text near 65 characters wide; set
a type scale and stay on it; give headings `text-wrap: balance`, body
text room to breathe, and uppercase labels a touch of letter-spacing.

**Choose neutrals, don't default to them.** A pure mid-grey reads as
unconsidered; a grey with a slight hue bias toward the page's accent
reads as chosen. Pure white and near-black are fine grounds when they
suit the subject — the point is that the neutral was picked, not
inherited.

**Let layout do the spacing.** Lay out sibling groups with flex or grid
and `gap`, not per-element margins that silently collapse or double.
Wide content — tables, code, diagrams — gets `overflow-x: auto` on its
own container so the page body never scrolls sideways. Reach for
`font-variant-numeric: tabular-nums` wherever digits line up in columns.

**Avoid AI-default design.** AI-generated design clusters around a few
looks: warm cream with a serif display and terracotta accent; near-black
with a lone acid-green or vermilion pop; broadsheet hairline rules with
dense columns; a purple-to-blue gradient hero on white; Inter or Space
Grotesk as the "safe" face; emoji as section markers; everything
centered; rounded cards with accent bars everywhere. Where the user
pins a visual direction, follow it exactly — their words always win.
Where nothing is specified, don't spend that freedom on a default.

**Build cleanly.** Watch for overlapping elements, cascade collisions,
and silent font fallbacks — visual bugs hide in the gap between source
and output. Close every non-void element, double-quote attributes, give
keyboard focus a visible state, respect `prefers-reduced-motion`. For
generative or decorative graphics, reach for Canvas rather than
hand-authoring long SVG path data. Watch selector specificity: a
type-based selector fighting an element-based one over padding silently
undoes your spacing.

**Words are design material.** Write from the user's side of the
screen — name things by what people recognize, not how the system is
built. Active voice; a control says exactly what happens. Errors explain
what went wrong and how to fix it. Specific beats clever.

**Structure is information.** Numbering, eyebrows, dividers, and labels
should encode something true about the content, not decorate it.
Numbered markers (01 / 02 / 03) are only appropriate when the content
actually is a sequence.

**When it's a UI, not a document.** A dashboard or tool is scanned and
operated, not read top-to-bottom, so the craft shifts to information
design. Surface the summary before the detail; encode state in form as
well as number — a pill, a chip, a severity stripe. Semantic color
(good / warning / critical) is separate from the accent hue and doesn't
count as your accent. Give charts the same care as type: an area fill,
a faint grid, an emphasized endpoint. What's interactive should look
interactive.

## Process

Before writing code, sketch a short design plan — a compact token
system:
- **Color**: the palette as 4-6 named values (both themes).
- **Type**: typefaces for 2+ roles — a characterful display face used
  with restraint, a complementary body face, a utility face for data.
- **Layout**: the layout concept in one or two sentences.

Then build, deriving every color and type decision from the plan.

When the request is editorial, the stance shifts: make opinionated
calls and take one real aesthetic risk where it serves the work. Review
the plan against the subject before building — if any part reads like
the generic default for any similar page, revise it. The hero is a
thesis: open with the most characteristic thing in the subject's world.
Use motion deliberately — one orchestrated moment lands harder than
scattered effects. Spend your boldness in one place; keep everything
around it quiet.
