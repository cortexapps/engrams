import { describe, expect, it } from "vitest";

import { oklchToHex, randomTagColor, TAG_QUICK_PICKS } from "./colors";

// Expected hex values come from an independent OKLab reference conversion,
// not from the code under test.
describe("oklchToHex", () => {
  it("converts known OKLCH values to sRGB hex", () => {
    expect(oklchToHex(1, 0, 0)).toBe("#ffffff");
    expect(oklchToHex(0, 0, 0)).toBe("#000000");
    expect(oklchToHex(0.627955, 0.257683, 29.2339)).toBe("#ff0000");
  });

  it("clamps out-of-gamut channels instead of wrapping", () => {
    expect(oklchToHex(1, 0.4, 0)).toMatch(/^#[0-9a-f]{6}$/);
    expect(oklchToHex(1.2, 0, 0)).toBe("#ffffff");
  });
});

describe("randomTagColor", () => {
  it("maps the random number onto the suggested hue band", () => {
    expect(randomTagColor(() => 0)).toBe("#009bab");
    expect(randomTagColor(() => 0.5)).toBe("#747dd3");
    expect(randomTagColor(() => 1)).toBe("#bf6291");
  });
});

describe("TAG_QUICK_PICKS", () => {
  it("has 8 distinct colors spanning the suggested hue band edge to edge", () => {
    expect(new Set(TAG_QUICK_PICKS).size).toBe(8);
    expect(TAG_QUICK_PICKS[0]).toBe("#009bab");
    expect(TAG_QUICK_PICKS.at(-1)).toBe("#bf6291");
  });
});
