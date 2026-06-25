import { describe, test, expect } from "vitest";

import { builtinLogo } from "./connectorLogos";

describe("builtinLogo", () => {
  test("resolves a bundled mark for the built-in providers", () => {
    // A sample across the catalog (incl. the underscore slug new_relic).
    for (const p of ["github", "datadog", "slack", "stripe", "new_relic", "figma"]) {
      expect(builtinLogo(p)).toBeTruthy();
    }
  });

  test("returns undefined for an unknown provider (→ falls through to the monogram)", () => {
    expect(builtinLogo("totally-unknown-provider")).toBeUndefined();
  });
});
