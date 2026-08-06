# Review notes — `web/`

## Authority

[`web/DESIGN.md`](../DESIGN.md) is the visual authority for this app. It names
the colors, the type roles, the spacing, and the shadows. Cite it by name.
If a change is right and `DESIGN.md` disagrees, the change updates `DESIGN.md`
too.

## Use the stock component

`web/src/components/ui/` holds the shadcn primitives. A hand-rolled tab row,
disclosure, card, or badge reports the wrong ARIA role, drops the keyboard
behavior, and drifts from the other copies of the same control. Stick to 
stock shadcn where possible.

Override a stock component to remove padding or to set a size. Its edge, its
radius, and its ground belong to the component.

## CSS traps

**An unlayered class beats every Tailwind utility.** Tailwind v4 puts
utilities in `@layer utilities`, and an unlayered rule outranks any layered
rule at any specificity. A custom class belongs in `@layer components`, or it
silently overrides the utilities that callers put on the element.

**A scroll property survives a change of `overflow`.** An element with
`overflow: hidden` is still a scroll container, so `scrollbar-gutter: stable`
still reserves its width. Overlay scrollbars hide this on macOS. When one rule
changes `overflow`, check what the other rules still reserve.

## Status and color

Status uses the instrument tokens, never a raw palette color. The work pane
re-tones its subtree, so a palette green that passes on white can fall below
3:1 inside it. Status rides a dot or a glyph, not the body text.

`text-ring` means "this is running". `text-primary` means "you can click
this".

## Prose and comments

Every user-visible string and every comment follows ASD-STE100 Simplified
Technical English, per `AGENTS.md`. A comment says what the code does now and
why. Git holds the history.

Comments should be self-contained, understood easily by future readers. They should
not reflect conversations in a session, reference files that don't exist.