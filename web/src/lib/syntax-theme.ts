import type { ThemeRegistration } from "shiki/types";

// The engrams syntax palette — two Shiki themes cut from the app's own colour
// system (web/DESIGN.md), one per ground.
//
// WHY NOT A STOCK THEME: the code block's chrome already speaks the system
// (celadon paper, green-black night, hairline rule, muted fill), but a stock
// theme's tokens do not. `github-dark-default` paints identifiers in #7ee787 —
// chartreuse, straight into the band our lime accent owns. Lime means "this is
// the action" and nothing else, so a syntax green that close dilutes it. The
// same distinction index.css already draws for status ("signal green — emerald,
// not lime") applies here.
//
// WHY NOT A BRAND-ONLY PALETTE: a reader parses code by hue CATEGORY before
// they read the token. Keyword ≈ violet, string ≈ green, number ≈ amber,
// comment ≈ faded — every serious product keeps those roles and re-tunes only
// the tone. So do we: conventional roles, our values.
//
// Six colours plus two neutrals. An editor theme carries twenty; a block set in
// prose reads calm with six, and everything unclaimed stays ink.
//
// CONTRAST: the `pre` ground is `bg-muted/30` over the thread background —
// #eef3e9 on paper, #0f2122 at night. Every colour below was picked in OKLCH at
// a fixed lightness band for that ground and measures 5.4:1 or better (AA at
// any size). Plain ink stays the darkest/brightest thing in the block, so the
// tokens read as a layer UNDER the identifiers, never louder than them.

type Role = "plain" | "comment" | "keyword" | "string" | "number" | "function" | "type" | "deleted";

type Palette = Record<Role, string> & { background: string };

// oklch(0.301 0.02 206) petrol ink · 0.43/0.026/200 ink-faded (--muted-foreground)
// · 0.47/0.16/330 violet · 0.46/0.12/150 emerald · 0.5/0.14/68 amber
// · 0.5/0.17/255 blue · 0.48/0.09/195 verdigris (--mark-terminal's hue)
// · 0.52/0.2/27 signal red (--instrument-critical-ink).
const LIGHT: Palette = {
  background: "#eef3e9",
  plain: "#223133",
  comment: "#3f5455",
  keyword: "#892f84",
  string: "#0f6a31",
  number: "#954f00",
  function: "#0060c1",
  type: "#006d6d",
  deleted: "#c2181d",
};

// The same seven hues lifted onto the green-black ground: L 0.74–0.82 where the
// light theme sits at 0.43–0.52. Hues hold, so a block does not change meaning
// when the theme flips — only its value does.
const DARK: Palette = {
  background: "#0f2122",
  plain: "#d8e3ca",
  comment: "#aab29e",
  keyword: "#e69fdb",
  string: "#7cd591",
  number: "#ebb76c",
  function: "#85beff",
  type: "#73d1ca",
  deleted: "#ff716b",
};

// One scope table, both themes. Ordered least-to-most specific: TextMate takes
// the LAST match, so a later row refines an earlier one.
const SCOPES: Array<[Role, string[]]> = [
  // Identifiers, operators and punctuation are the page, not the highlight.
  // Leaving them at plain ink is what keeps six colours from reading as twenty.
  [
    "plain",
    ["variable", "variable.other", "variable.parameter", "punctuation", "keyword.operator"],
  ],
  ["comment", ["comment", "punctuation.definition.comment"]],
  [
    "keyword",
    [
      "keyword",
      "keyword.control",
      "keyword.operator.expression",
      "keyword.operator.new",
      "storage",
      "storage.type",
      "storage.modifier",
      "variable.language", // self / this / super
      "entity.name.tag", // HTML, JSX, XML
    ],
  ],
  [
    "string",
    [
      "string",
      "string.quoted",
      "string.template",
      "constant.other.symbol", // Ruby :symbols
      "markup.inserted", // a ```diff fence
    ],
  ],
  [
    "number",
    [
      "constant.numeric",
      "constant.language", // true / false / null / nil
      "constant.character",
      "constant.character.escape",
      "support.constant",
    ],
  ],
  [
    "function",
    [
      "entity.name.function",
      "meta.function-call",
      "support.function",
      "support.type.property-name", // JSON keys, CSS properties
      "markup.heading",
    ],
  ],
  [
    "type",
    [
      "entity.name.type",
      "entity.name.class",
      "entity.name.namespace",
      "entity.other.inherited-class",
      "entity.other.attribute-name",
      "support.type",
      "support.class",
    ],
  ],
  ["deleted", ["markup.deleted", "invalid", "invalid.illegal"]],
];

// Emphasis carried by weight/slant rather than a seventh colour.
const FONT_STYLE: Partial<Record<Role, string>> = { comment: "italic" };

function build(name: string, type: "light" | "dark", palette: Palette): ThemeRegistration {
  return {
    name,
    type,
    colors: {
      "editor.foreground": palette.plain,
      "editor.background": palette.background,
    },
    fg: palette.plain,
    bg: palette.background,
    settings: [
      { settings: { foreground: palette.plain, background: palette.background } },
      ...SCOPES.map(([role, scope]) => ({
        scope,
        settings: {
          foreground: palette[role],
          ...(FONT_STYLE[role] ? { fontStyle: FONT_STYLE[role] } : {}),
        },
      })),
    ],
  };
}

export const ENGRAMS_LIGHT_THEME = build("engrams-light", "light", LIGHT);
export const ENGRAMS_DARK_THEME = build("engrams-dark", "dark", DARK);
