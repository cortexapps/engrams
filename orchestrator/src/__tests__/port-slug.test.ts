/**
 * Vanity-slug generator (ADR 0064): shape + DNS-label safety.
 */

import { expect, test, describe } from "bun:test";

import { generateSlug, isValidSlug } from "../ports/slug.ts";

describe("port slug generator", () => {
  test("generates an adjective-adjective-noun triple of lowercase words", () => {
    for (let i = 0; i < 200; i++) {
      const slug = generateSlug();
      const parts = slug.split("-");
      expect(parts.length).toBe(3);
      for (const p of parts) {
        expect(p).toMatch(/^[a-z]+$/);
      }
      // Always a valid DNS label.
      expect(isValidSlug(slug)).toBe(true);
      expect(slug.length).toBeLessThanOrEqual(63);
    }
  });

  test("has enough entropy to rarely collide across a handful of mints", () => {
    const seen = new Set<string>();
    for (let i = 0; i < 50; i++) seen.add(generateSlug());
    // 50 mints from ~150k combinations: collisions are very unlikely. Allow a
    // tiny margin so this never flakes (the store retries real PK collisions).
    expect(seen.size).toBeGreaterThanOrEqual(48);
  });

  test("isValidSlug rejects non-DNS-label inputs", () => {
    expect(isValidSlug("Jumping-Fat-Kittens")).toBe(false); // uppercase
    expect(isValidSlug("-leading")).toBe(false);
    expect(isValidSlug("trailing-")).toBe(false);
    expect(isValidSlug("has.dot")).toBe(false);
    expect(isValidSlug("has space")).toBe(false);
    expect(isValidSlug("a".repeat(64))).toBe(false); // > 63
    expect(isValidSlug("ok-slug-3000")).toBe(true);
  });
});
