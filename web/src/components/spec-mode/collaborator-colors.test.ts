import { describe, expect, test } from "vitest";

import { collaboratorColor } from "./collaborator-colors";

describe("collaboratorColor", () => {
  test("is stable for the same user id", () => {
    expect(collaboratorColor("user-42")).toBe(collaboratorColor("user-42"));
  });

  test("assigns only six-digit awareness-safe identity colors", () => {
    const colors = new Set(
      Array.from({ length: 200 }, (_, index) => collaboratorColor(`u${index}`)),
    );
    expect(colors).toHaveLength(6);
    for (const color of colors) expect(color).toMatch(/^#[0-9a-f]{6}$/);
  });

  test("includes the two explicit collaborator colors from the design handoff", () => {
    const colors = new Set(
      Array.from({ length: 200 }, (_, index) => collaboratorColor(`u${index}`)),
    );
    expect(colors).toContain("#b85c0a");
    expect(colors).toContain("#3a6b5c");
  });
});
