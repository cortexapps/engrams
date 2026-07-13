import { expect, test } from "bun:test";

import { tools } from "../registry.ts";

test("every production session-handled tool has a presenter or explicit exemption", () => {
  const violations = tools
    .all()
    .filter((tool) => tool.handling === "session")
    .filter((tool) => Object.keys(tool.presenters ?? {}).length === 0)
    .filter((tool) => !tool.presenterExempt?.trim())
    .map((tool) => tool.name);

  expect(violations).toEqual([]);
});
