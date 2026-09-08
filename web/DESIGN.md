---
name: engrams
description: A logbook page lifted off its cover — the agent's work is the only lit surface.
colors:
  lime: "oklch(0.878 0.181 121)"
  lime-ink: "oklch(0.22 0.04 150)"
  paper: "oklch(0.964 0.013 128)"
  sheet: "oklch(0.995 0.005 128)"
  ink: "oklch(0.301 0.02 206)"
  ink-muted: "oklch(0.43 0.026 200)"
  rule: "oklch(0.898 0.013 128)"
  ring: "oklch(0.55 0.15 132)"
  cover: "oklch(0.315 0.045 190)"
  cover-deep: "oklch(0.222 0.038 190)"
  cover-raised: "oklch(0.378 0.048 190)"
  pane: "oklch(0.365 0.046 190)"
  pane-card: "oklch(0.415 0.05 190)"
  pane-track: "oklch(0.322 0.043 190)"
  pane-active: "oklch(0.512 0.053 190)"
  sage: "oklch(0.945 0.028 125)"
  sage-muted: "oklch(0.845 0.031 125)"
  nominal: "oklch(0.6 0.15 150)"
  caution: "oklch(0.7 0.155 71)"
  critical: "oklch(0.55 0.21 27)"
  nominal-ink: "oklch(0.52 0.14 150)"
  caution-ink: "oklch(0.5 0.13 71)"
  critical-ink: "oklch(0.52 0.2 27)"
typography:
  display:
    fontFamily: "'Saira Variable', system-ui, sans-serif"
    fontSize: "26px"
    fontWeight: 600
    lineHeight: 1.2
    fontStretch: "108%"
  masthead:
    fontFamily: "{typography.display.fontFamily}"
    fontSize: "20px"
    fontWeight: 600
    lineHeight: 1.3
  sans:
    fontFamily: "system-ui, -apple-system, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif"
  body:
    fontFamily: "{typography.sans.fontFamily}"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.45
  prose:
    fontFamily: "{typography.sans.fontFamily}"
    fontSize: "16px"
    fontWeight: 400
    lineHeight: 1.55
  label:
    fontFamily: "{typography.sans.fontFamily}"
    fontSize: "12px"
    fontWeight: 600
    lineHeight: 1.3
    letterSpacing: "normal"
  mono:
    fontFamily: "'JetBrains Mono Variable', ui-monospace, 'SF Mono', Menlo, monospace"
    fontSize: "12px"
    fontWeight: 400
    fontFeature: "tabular-nums"
rounded:
  sm: "8px"
  md: "10px"
  lg: "12px"
  xl: "16px"
spacing:
  gutter: "8px"
  tight: "12px"
  group: "20px"
components:
  button-primary:
    backgroundColor: "{colors.lime}"
    textColor: "{colors.lime-ink}"
    rounded: "{rounded.md}"
    padding: "0 16px"
    height: "36px"
    typography: "{typography.label}"
  button-ghost:
    backgroundColor: "transparent"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "0 12px"
    height: "32px"
  card:
    backgroundColor: "{colors.sheet}"
    textColor: "{colors.ink}"
    rounded: "{rounded.lg}"
    padding: "12px"
  card-on-pane:
    backgroundColor: "{colors.pane-card}"
    textColor: "{colors.sage}"
    rounded: "{rounded.md}"
    padding: "12px"
  input:
    backgroundColor: "{colors.sheet}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "0 12px"
    height: "36px"
  tab-active:
    backgroundColor: "{colors.pane-active}"
    textColor: "{colors.sage}"
    rounded: "{rounded.sm}"
    padding: "0 10px"
    height: "28px"
  rail-item-active:
    backgroundColor: "{colors.cover-raised}"
    textColor: "{colors.sage}"
    rounded: "{rounded.md}"
    padding: "0 8px"
    height: "32px"
---

# Design System: engrams

<!-- Established in the /proto prototype (web/src/proto), world "C · Graphite
     green" with rail=spine, lift=panes, pane=cover. Every colour pair below
     was measured, not judged by eye. -->

## Overview

**Creative North Star: "The Page on the Cover"**

engrams is a logbook that has been opened. The chrome is the cover — one
continuous bottle-green field that holds the destinations, the session list and
the ground between them. The work is the page — a celadon sheet lifted off that
cover, carrying the agent's thread. Nothing else in the interface is allowed to
be lit.

The system inherits the palette engrams already shipped (celadon paper, petrol
ink, racing-green-black, Aston lime) and replaces how that palette was arranged.
The previous arrangement announced itself before it spoke: a tracked-caps kicker
over every page, an explanatory sentence under every title, a hand-rolled tab
rule, and an 8px corner on everything regardless of what it held. Those are
retired. Structure now comes from **planes and space**, not from labels and
hairlines.

Density is high and unapologetic — the audience reads digests, ids and diff
counts all day — but density is bought with hierarchy, not with compression.
Confirmed anti-references: the nested-panel cloud console, and the toy consumer
app. A third is now specific to this system: **any interface that separates
regions with rules instead of with surfaces.**

**Key Characteristics:**

- One cover, two depths; one lit work surface.
- Sentence case everywhere. No tracked caps, ever.
- Lime is action, never status.
- Cards have edges. Bands do not.
- Stock shadcn until a real reason forces a fork.

## Colors

A single ground hue split across two materials: paper on the work surface,
bottle-green on everything that frames it. Lime is the only saturated colour
that acts.

### Primary

- **Aston Lime** (`{colors.lime}`): interactive accent only — the primary
  button, the send control, the active rail marker, the focus ring on dark
  ground, and the selection wash. It never carries state, never fills a badge
  that means "healthy", and never appears as body text on paper (chartreuse on
  celadon is invisible).

### Secondary

- **Bottle Green** (`{colors.cover}`): the cover. The destinations rail, the
  session list and the ground behind the sheets are all this value.
- **Cover Deep** (`{colors.cover-deep}`): the destinations rail only — the
  outermost chrome, one step below the cover so the two navs stay distinct.
- **Pane Green** (`{colors.pane}`): the work pane, cut from the cover and raised
  onto the page. Its cards (`{colors.pane-card}`), tab track
  (`{colors.pane-track}`) and active tab (`{colors.pane-active}`) step around it.

### Neutral

- **Celadon Paper** (`{colors.paper}`): the thread's ground — the lit surface.
- **Clean Sheet** (`{colors.sheet}`): cards and inputs on paper. Lighter than the
  page, so a card lifts without needing a border to announce it.
- **Petrol Ink** (`{colors.ink}`) / **Faded Ink** (`{colors.ink-muted}`): body
  and secondary text on paper.
- **Sage** (`{colors.sage}`) / **Faded Sage** (`{colors.sage-muted}`): body and
  secondary text on every green ground — cover, rails, work pane.
- **Hairline** (`{colors.rule}`): borders and dividers, deliberately low
  contrast. A rule delimits an object; it never divides a region.

### Tertiary

Instrument status keeps its own vocabulary, strictly apart from lime:
**nominal** (`{colors.nominal}`), **caution** (`{colors.caution}`), **critical**
(`{colors.critical}`). Status is always glyph or text plus colour, never colour
alone.

Each signal ships in **two** values, and picking the wrong one is the most
common way to fail contrast here. The plain token is the *graphic* value —
meter fills and 6px dots — tuned to stay vivid at small sizes against
a 3:1 floor. The `-ink` token (`{colors.nominal-ink}`, `{colors.caution-ink}`,
`{colors.critical-ink}`) is the *text* value, dark enough on paper and light
enough at night to clear 4.5:1. Measured as text on paper, the graphic values
land at 3.34, 2.48 and 4.88 — two of the three fail. Anything that colours a
word or a figure takes the ink.

The one place the two converge is inside `.work-pane`: the ground there is dark
in both themes, so a single set clears both floors.

**Running** is not one of these three. It has its own tone, `{colors.ring}`,
set by `Glyph`'s `toneFor` and by `StatusDot`'s `active` — racing green on
paper, lime at night. That is the deliberate exception to the Lime-Is-Action
rule, and the only one.

**Thresholds live with the primitive**, not the caller (`components/meter.tsx`,
`operator-health.ts`): a usage meter (disk, memory, cpu, capacity) is nominal
below 70%, caution from 70%, critical from 90%; base locality is nominal from
80%, caution from 50%, critical below; a last flush is nominal inside the 60s
window and caution past it — staleness, never recency. `--destructive` is for
destructive actions, not for a meter.

### Named Rules

**The One Cover Rule.** The destinations rail, the session list and the ground
between the sheets are one continuous backing. If a region is not the work
surface, it is the cover — it does not get a background of its own.

**The Lime-Is-Action Rule.** Lime marks what you can do. The moment it marks
what something *is*, the status vocabulary has been broken. The one sanctioned
exception is `Glyph`'s running tone, which is `{colors.ring}` — and `ring`
resolves to lime only at night, where it is the accent doing double duty on a
dark ground.

**The Ink-For-Text Rule.** A status colour applied to a word or a figure uses
the `-ink` variant, never the graphic one. Reach for the plain token only when
the thing being coloured is a dot, an arc, or a fill.

**The Green-Ground Recheck Rule.** Any status colour placed on a green surface
is re-measured against `{colors.pane-card}` — the lightest ground that text ever
lands on — not against the pane. Paper-era signal red fails this outright: it
needs lifting from `0.52` to `0.85` lightness because red goes *muddy* on green,
not merely dim.

## Typography

**Display Font:** Saira Variable — page titles only, 600, set 8% wide
**Body / Label Font:** system UI stack
**Machine Font:** JetBrains Mono Variable

**Character:** One workhorse family carries chrome, headings and prose; the
display face appears exactly once per page, on the title (`Text
variant="display"`). The borrowed, generated feel came from tracked caps on
every label, not from the face, so Saira keeps the title and loses everything
else. Monospace is reserved for things a machine produced — ids, digests,
paths, branch names, byte counts, diff numbers, code.

### The type scale

Seven sizes, as Tailwind utilities, and every size in the app is one of them.
Arbitrary `text-[…]` values are banned.

| Utility | px | Use |
|---|---|---|
| `text-2xs` | 11 | mono ages, counts, ids in dense rows; rail meta |
| `text-xs` | 12 | captions, field labels, table heads, secondary lines |
| `text-sm` | 14 | body — rows, buttons, descriptions |
| `text-base` | 16 | prose (transcript) and inputs (keeps iOS from zooming a field) |
| `text-lg` | 20 | a section masthead (a builder's name) |
| `text-xl` | 26 | the page title |
| `text-2xl` | 30 | the title of a centered composer page |

### Hierarchy

- **Display** (Saira 600, 26px, 108% width): page titles. Paired with a count
  chip rather than a subtitle. 30px on a centered composer page.
- **Heading** (600, 14px): section and card headings, sentence case — never a
  tracked kicker, never `Text variant="label"` used as a heading.
- **Body** (400, 14px): rows and descriptions. Prose in the transcript is 16px;
  measure caps at roughly 65–75 characters.
- **Label** (600, 12px): field captions and table heads, in sentence case.
- **Mono** (400, 12px or 11px, tabular): all machine data.

### Named Rules

**The Sentence-Case Rule.** No uppercase, no letter-spacing above `normal`,
anywhere. Not on buttons, not on tabs, not on labels, not on table headers.

**The Louder-Than-Its-Contents Rule.** A group label outranks the rows beneath
it — heavier and darker, never a faint 60%-opacity caption. A heading quieter
than its own contents divides nothing.

**The Mono-Means-Machine Rule.** Monospace is for data a machine emitted. It is
never a costume for "technical".

**The One Clock Rule.** Every relative time comes from `lib/relative-time`:
`relativeTime` for the sentence voice ("4m ago", "in 2h", "just now", "never")
and `relativeAge` for dense columns ("3m"). No surface appends " ago" by hand.

## Layout

The shell is a fixed-height frame that clips its own overflow; only the work
surface scrolls. Left to right: the spine (208px, collapsible to an icon rail),
the section rail (272px; the Tasks rail is drag-resizable), then the work
surface. One component, `SectionLayout`, builds that frame for every section;
a section supplies its rail and its page and nothing else.

The spine holds products only — Tasks, Reviews, Artifacts, Automations — and
Settings as a normal row at its foot, above the avatar. Everything an admin
configures (the workspace, the runtime, the fleet, storage, papercuts) is a
Settings page in one of five flat groups; there is no Operator hat and no
one-item section.

The work surface sits in an **8px gutter** on all four sides, so the cover shows
through evenly around it. In the session view it splits into two sheets — the
thread and the work pane (432px) — separated by the same 8px gutter.

Spacing rhythm: 12px inside an object, 8px between siblings, 20px above a group
heading against 8px below it. Groups separate by space and a hairline, never by
a colour change.

Below `xl`, the work pane leaves the split and becomes an overlay; the thread
takes the full width. Below `md`, both rails become off-canvas sheets.

## Elevation & Depth

This system is **layered, and lifted exactly once.** Depth is carried by tonal
planes — five of them, from the deepest rail to the lit page — and shadow is
spent only on the work surface. Everything else is flat.

The ladder, deepest to lightest (light theme):

1. Destinations rail — `{colors.cover-deep}`
2. Cover: session list and ground — `{colors.cover}`
3. Work pane — `{colors.pane}`, raised
4. Thread — `{colors.paper}`, raised
5. Cards on the thread — `{colors.sheet}`

### Shadow Vocabulary

- **Lift** (`box-shadow: 0 1px 2px oklch(0 0 0 / 0.16), 0 10px 30px -8px oklch(0 0 0 / 0.3)`):
  the only shadow in the system. Two layers — a tight contact shadow that seats
  the sheet on the cover, and a wide soft cast that lifts it. Dark theme deepens
  both and adds a `oklch(1 0 0 / 0.09)` top edge, the way a raised surface
  catches light.

### Named Rules

**The Lift-Means-Work Rule.** Only the thread and the work pane are elevated.
A card, a dialog, a menu or a rail that borrows the lift dilutes the one signal
that says "this is the work".

**The Offset Rule.** Every shadow carries a downward offset. A zero-offset
shadow is a glow, and a glow reads as focus or error, not as height.

## Shapes

Corners are **gently rounded and consistent**: 12px (`rounded-lg`) on lifted
surfaces and cards, 10px (`rounded-md`) on controls, 8px (`rounded-sm`) on
small buttons, tab pills and chips that describe (`built-in`, a tag),
full-round only on counts and status badges. The previous 8px-everywhere
corner made a 400px card and a 24px button read as the same kind of object.

**The Pills-Never-Act Rule.** A full-round shape is a readout — a count, a
state. A button, a link or a filter that is shaped like one promises to be
read, not pressed.

Borders are hairlines at low contrast. Their job is to close an object, not to
build a grid. Where a surface can be distinguished by fill, it is — the border
is then only an edge, not the separator.

### Named Rules

**The Card-Has-Edges Rule.** Anything presented as an object carries a border
*and* a radius *and* its own padding. A block that bleeds to both edges of its
container with a single `border-bottom` is chrome, and will be read as chrome.

**The One Divider Rule.** The shell separates its planes by fill and by the 8px
gutter, and it draws exactly one divider: the hairline on the section rail's
left edge, where the two rails touch with no gutter between them. That one line
exists because the dark theme cannot afford the value step. The page sits at
`#0b1d1e`, leaving about eleven sRGB levels beneath it for the pane, the shell,
and the destinations rail; push the rail far enough down to match the light
theme's 0.093 step and it renders `#000506` — black, with the green gone. So
the dark rails take 0.040 and the hairline carries the rest. Judge every dark
separation in OKLCH lightness, never in WCAG ratio: the ratio's flare term
compresses the whole near-black range, and reports a clearly visible step as
1.06:1.

## Components

The rule above all others: **use stock shadcn.** The components in `web/src/components/ui`
are the vocabulary. A fork needs a stated reason, and "our brand" is not one —
the previous fork put tracked caps into `button.tsx` and `badge.tsx` and cost
the whole app its sentence case.

### Buttons

- **Shape:** gently rounded (10px), 36px tall at default, 32px small.
- **Primary:** lime fill, deep racing-green ink, sentence case, medium weight.
- **Ghost / Outline:** the default for anything in a rail, a masthead or a pane
  header. Ghost carries icon-only controls.
- **Hover / Focus:** hover lightens the accent wash; focus is a 2px ring offset
  from a 2px background halo, not a 3px blur.

### Chips

- **Counts and states:** full-round, muted fill, `{typography.mono}` for the
  count, sentence case for the word. The count beside a page title; a status
  badge.
- **Descriptors:** 8px corners (`built-in`, a tag, a category). Never lime:
  `Badge`'s default variant says "you can do this", not "this is what it is".
- A chip never carries an action.

### Status dots, meters, empty and loading states

One component each, in `components/`:

- **`StatusDot`** — the only dot. Tones nominal / caution / critical / active
  (running, `{colors.ring}`) / muted; 6px in dense rows, 8px in lists, 10px on
  a masthead. Always beside a word.
- **`Meter`** — a 6px bar in an instrument tone, with the thresholds above.
  `Progress` is for work in flight (the running tone), never for a reading.
- **`EmptyState`** — a dashed card holding one quiet sentence and at most one
  action; `tone="error"` adds the alert glyph in ink; `inline` inside a surface
  that already is a card. The only empty idiom.
- **`SkeletonRows`** — placeholder bars in the column geometry of the coming
  data. The only loading idiom; there is no "Loading…" string.

### Cards / Containers

- **Corner:** 12px on paper, 10px on the pane.
- **Background:** `{colors.sheet}` on paper, `{colors.pane-card}` on the pane.
- **Shadow:** none. Cards separate by fill and border; only the work surface
  lifts.
- **Border:** 1px hairline, always.
- **Padding:** 12px.

### Inputs / Fields

- **Style:** sheet fill, hairline border, 10px radius, 36px tall.
- **Composer:** the one exception — a card-sized container (12px) whose focus
  ring belongs to the whole object, so the borderless textarea and its control
  row read as one input.
- **Focus:** ring in `{colors.ring}` on paper, lime on green.

### Navigation

- **Spine:** icon plus sentence-case label, 34px rows, 10px radius. Active is
  the raised wash, a heavier weight, and a 3×16px lime bar at the spine's edge —
  the one place lime marks a place rather than an action.
- **Section rail:** 32px rows, 10px radius, sentence case. Group headers are
  12px semibold in full ink with an optional mono count on the right.
- **Session list:** grouped rows on the cover. Group headers are label-weight
  with a count chip, separated by 20px of space and a hairline. A row carries a
  status glyph, a title, an age, and an optional one-line note.
- **Tabs:** stock shadcn `Tabs`, styled as pill tabs — 30px, 8px corners, no
  track, the active pill on `--secondary` at 600 with no shadow. On the pane,
  `.work-pane`'s own `--secondary` keeps the step visible.

### The Work Pane

The reference surface, cut from the cover and raised onto the page. In light
theme it is the system's signature move: a bright thread flanked by dark chrome
on both sides, so the agent's work is unmistakably the subject.

Because its ground is green, the pane re-declares its **whole token set** —
ink, cards, borders, and all four status hues — scoped to `.work-pane`. This is
the same technique `.sidebar-section` uses in `index.css`. A background-only
override would leave petrol ink on bottle green.

## Motion

Quiet and purposeful: one easing (`cubic-bezier(.2,.7,.2,1)`), a handful of
durations, and every animation honours `prefers-reduced-motion` (instant, or
already drawn). Nothing scales on press. Nothing glows. Rails never move after
boot. The vocabulary lives in `index.css` under "Motion".

- **Boot** (once per load, never on a route change): the sheet rises 14px and
  fades in over 700ms after a 200ms beat; rail rows follow, 40ms apart; the
  mark draws on. `RootLayout` carries `data-boot` for 1.4s and then detaches it.
- **Page in**: on a route change the sheet's body rises 6px and fades in over
  240ms (`SectionPage` re-keys on the path). Rails do not move.
- **Tabs**: the active pill slides between triggers in 180ms (one framer
  `layoutId` per list). **Hover** is the accent wash in 120ms; **focus** is the
  2px ring in `--ring` (kept from the earlier measurement; the handoff's 3px
  reads as a halo on a 12px corner).
- **Live**: a running dot or glyph breathes 1 → .35 → 1 over 2.4s
  (`.animate-live`); a running row carries a 34%-wide `--ring`-tinted sweep
  every 2.8s (`.row-running`). A changed figure ticks — old up and out, new in
  from below — in 500ms (`TickNumber`).
- **Data ink**: a meter fills 0 → value over 1.1s, 150ms per meter (`Meter
  index`); a sparkline's bars draw once on mount; a skeleton's
  hairlines draw over 900ms, 150ms apart, then its bars shimmer at 1.8s.
- **Dark**: identical moves; the sheet adds its top edge. No glows anywhere.

## Logomark

The engram trace has one silhouette: the path `M14 68 L32 50 L50 68 L68 32
L86 50` in a 100-unit box, round caps and joins, in `currentColor`. Two nodes
carry meaning by shape — the entry RING (amber, `--mark-entry`; `#e0913d` on
the cover) at the start and the terminal DOT (the ground's action colour:
`--mark-terminal` verdigris on paper, lime on the cover and at night) at the
end. No middle nodes, no lattice, no shape-swapping. Stroke 9 at 64px and up,
10 at 32px, 12 at 16px; below 20px the ring is dropped.

- **Static** is the default. **Draw-on** runs once, at boot or resume: the
  ring is present from the first frame, the path draws over 900ms, the dot
  lands in 240ms. The mark is identity, not a status light — nothing pulses it.
- **Loader**: the trace ghosted at 24% with a 60-unit lit segment travelling
  it every 1.8s — for the connection states the app shows (a pane connecting,
  the auth stage). Lime on the spine, ink on paper.
- **Wordmark**: lowercase `engrams`, sans 600, `letter-spacing: -0.02em`, the
  mark at cap height, gap = the stroke width.
- **Monochrome** (`mono`): ring and dot in one colour; the shapes still read.
- **App icon** (`web/scripts/gen-icons.mjs`): a tile with a 23% radius, the
  bottle-green gradient, a carbon twill at 10%, a light top edge and a dark
  bottom edge; the trace inset 16% in sage, the amber ring, the lime dot. At
  16px: stroke 13, dot r 11, no ring. The script writes the SVG, the PNGs and
  the ICO into `web/public` and mirrors them into the site.

## Do's and Don'ts

### Do:

- **Do** give a page a title and a count chip. That is the whole masthead.
- **Do** separate groups with space (20px above, 8px below) and a hairline.
- **Do** put every colour pair through a contrast check when it lands on green,
  measured against the lightest ground the text can reach.
- **Do** keep the composer's controls to what is still decidable. Inside a
  running session the profile, mode and harness are settled at launch — state
  them as detail text; do not offer a picker that changes nothing.
- **Do** reach for the stock shadcn component first, every time.

### Don't:

- **Don't** add an eyebrow. A tracked-caps kicker over a title
  (`Kaizen · Agent feedback`, `Org · Connectors`) is banned outright.
- **Don't** add an explanatory sentence under a page title. If the page needs
  explaining, the title is wrong.
- **Don't** use uppercase or positive letter-spacing anywhere.
- **Don't** separate a region with a rule when a surface would do it.
- **Don't** elevate anything except the thread and the work pane.
- **Don't** hand-roll a component that shadcn ships. The retired `TabRow` is the
  cautionary example: it existed only because stock `Tabs` was never tried.
- **Don't** use lime for status, or an instrument colour for an action.
- **Don't** invent a size (`text-[0.7rem]`), a dot, an empty state, a loader,
  or a relative-time format. Each has one home.
- **Don't** animate outside the motion vocabulary, and never without a
  reduced-motion fallback. Nothing scales on press; nothing glows.
