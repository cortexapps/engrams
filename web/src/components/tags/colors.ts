/** The colors the app suggests for a tag: the quick picks and the random
 * color. An admin can still enter any color; these only seed the choice. */

/** OKLCH hues the suggestions use. The band stays clear of the status hues
 * (critical 27, caution 71, nominal 150) and the accent hues (lime 121,
 * verdigris 178), so a suggested tag never reads as a state or an action. */
export const TAG_HUE_RANGE = [205, 350] as const;

/** One lightness and strength for every suggestion: the chip redraws any
 * color at fixed lightness per theme, so only the hue shows. */
const SUGGESTED_L = 0.62;
const SUGGESTED_C = 0.13;

/** OKLCH to lowercase `#rrggbb`, with out-of-gamut channels clamped (OKLab
 * matrices from Björn Ottosson's reference). */
export function oklchToHex(l: number, c: number, h: number): string {
  const rad = (h * Math.PI) / 180;
  const a = c * Math.cos(rad);
  const b = c * Math.sin(rad);
  const lc = (l + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const mc = (l - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const sc = (l - 0.0894841775 * a - 1.291485548 * b) ** 3;
  const linear = [
    4.0767416621 * lc - 3.3077115913 * mc + 0.2309699292 * sc,
    -1.2684380046 * lc + 2.6097574011 * mc - 0.3413193965 * sc,
    -0.0041960863 * lc - 0.7034186147 * mc + 1.707614701 * sc,
  ];
  return `#${linear
    .map((x) => {
      const v = Math.min(1, Math.max(0, x));
      const encoded = v <= 0.0031308 ? 12.92 * v : 1.055 * v ** (1 / 2.4) - 0.055;
      return Math.round(encoded * 255)
        .toString(16)
        .padStart(2, "0");
    })
    .join("")}`;
}

function suggestedColor(fraction: number): string {
  const [from, to] = TAG_HUE_RANGE;
  return oklchToHex(SUGGESTED_L, SUGGESTED_C, from + fraction * (to - from));
}

/** A random suggested color. `random` returns a number in [0, 1]. */
export function randomTagColor(random: () => number = Math.random): string {
  return suggestedColor(random());
}

/** Eight suggestions spread evenly across the hue band, edge to edge. */
export const TAG_QUICK_PICKS: readonly string[] = Array.from({ length: 8 }, (_, i) =>
  suggestedColor(i / 7),
);
