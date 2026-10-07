import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

// The accent is the one token that changes value per GROUND rather than per
// theme (web/DESIGN.md, the Accent-Follows-Its-Ground rule), which is exactly
// what makes it easy to break: a scope that puts a green ground inside the
// light theme has to re-declare the pair, and forgetting costs ~1.3:1 with no
// type error and no failing test. That has now happened twice — ADR 0100
// records amber-as-text at 2.5:1 on paper, "caught by a critique pass, not by
// the type checker" — so the ratios are asserted here instead.
//
// This reads index.css rather than a duplicated table of values, so the test
// cannot drift from the stylesheet it is guarding.

const CSS = readFileSync(join(__dirname, "index.css"), "utf8");

/** Pull one `selector { … }` block's custom properties out of the stylesheet. */
function scope(selector: string): Record<string, string> {
  const at = CSS.indexOf(`\n${selector} {`);
  if (at === -1) throw new Error(`no ${selector} block in index.css`);
  const open = CSS.indexOf("{", at);
  let depth = 0;
  let close = open;
  for (let i = open; i < CSS.length; i++) {
    if (CSS[i] === "{") depth++;
    else if (CSS[i] === "}" && --depth === 0) {
      close = i;
      break;
    }
  }
  const vars: Record<string, string> = {};
  for (const [, name, value] of CSS.slice(open, close).matchAll(
    /(--[\w-]+):\s*(oklch\([^)]*\))\s*;/g,
  )) {
    vars[name] = value;
  }
  return vars;
}

/** oklch(L C H) → linear sRGB. Only the opaque three-component form is used
 *  by the tokens under test; an alpha form would need a ground to composite
 *  against, so it is rejected rather than silently mis-measured. */
function linearRgb(oklch: string): [number, number, number] {
  const m = /^oklch\(([\d.]+)\s+([\d.]+)\s+([\d.]+)\)$/.exec(oklch.trim());
  if (!m) throw new Error(`not an opaque oklch() colour: ${oklch}`);
  const [L, C, h] = [Number(m[1]), Number(m[2]), (Number(m[3]) * Math.PI) / 180];
  const a = C * Math.cos(h);
  const b = C * Math.sin(h);
  const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const mm = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
  return [
    4.0767416621 * l - 3.3077115913 * mm + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * mm - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * mm + 1.707614701 * s,
  ];
}

function contrast(a: string, b: string): number {
  const lum = (c: string) => {
    const [r, g, bl] = linearRgb(c).map((v) => Math.min(Math.max(v, 0), 1));
    return 0.2126 * r + 0.7152 * g + 0.0722 * bl;
  };
  const [hi, lo] = [lum(a), lum(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

const root = scope(":root");
const dark = scope(".dark");
const workPane = scope(".work-pane");
const darkWorkPane = scope(".dark .work-pane");

/** Every (theme, ground) combination the accent is actually painted on.
 *  `text` is the ground a `text-primary` word lands on. Inside `.work-pane`
 *  that is the CARD, not `--background`: the pane repurposes `--background`
 *  as its lifted active-tab fill, which paints its own foreground (see the
 *  Green-Ground Recheck Rule in web/DESIGN.md). */
const GROUNDS = [
  { name: "light page", vars: root, text: ["--background", "--card"] },
  { name: "light work pane", vars: { ...root, ...workPane }, text: ["--card"] },
  { name: "dark page", vars: { ...root, ...dark }, text: ["--background", "--card"] },
  {
    name: "dark work pane",
    vars: { ...root, ...dark, ...workPane, ...darkWorkPane },
    text: ["--card"],
  },
] as const;

describe("the accent clears AA on every ground it lands on", () => {
  for (const { name, vars, text } of GROUNDS) {
    for (const ground of text) {
      it(`${name}: --primary as text on ${ground}`, () => {
        expect(contrast(vars["--primary"], vars[ground])).toBeGreaterThanOrEqual(4.5);
      });
    }

    it(`${name}: --primary-foreground on a --primary fill`, () => {
      expect(contrast(vars["--primary"], vars["--primary-foreground"])).toBeGreaterThanOrEqual(4.5);
    });
  }
});

describe("the accent stays out of the instrument vocabulary", () => {
  // Lime is interactive-only. If the accent drifts into a status hue, a user
  // cannot tell "you can do this" from "this is healthy" by colour alone.
  const hue = (c: string) => Number(/^oklch\([\d.]+\s+[\d.]+\s+([\d.]+)\)$/.exec(c.trim())![1]);
  const STATUS = ["--instrument-nominal", "--instrument-caution", "--instrument-critical"];

  for (const { name, vars } of GROUNDS) {
    it(`${name}: --primary is >15° from every instrument hue`, () => {
      const accent = hue(vars["--primary"]);
      for (const token of STATUS) {
        const delta = Math.abs(((accent - hue(vars[token]) + 540) % 360) - 180);
        expect(180 - delta).toBeGreaterThan(15);
      }
    });
  }
});
