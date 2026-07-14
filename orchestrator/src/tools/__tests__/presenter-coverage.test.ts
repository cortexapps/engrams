import { expect, test } from "bun:test";

import { registerBuiltinTools } from "../builtin.ts";
import { createToolRegistry } from "../registry.ts";

test("every production session-handled tool has a presenter or explicit exemption", () => {
  const registry = createToolRegistry();
  registerBuiltinTools(registry);
  const violations = registry
    .all()
    .filter((tool) => tool.handling === "session")
    .filter((tool) => Object.keys(tool.presenters ?? {}).length === 0)
    .filter((tool) => !tool.presenterExempt?.trim())
    .map((tool) => tool.name);

  expect(violations).toEqual([]);
});
