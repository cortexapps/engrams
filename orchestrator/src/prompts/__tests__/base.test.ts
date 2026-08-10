import { describe, expect, test } from "bun:test";

import { BASE_SYSTEM_PROMPT, SPEC_MODE_SYSTEM_PROMPT } from "../base.ts";

describe("base system prompt", () => {
  test("gives every harness the live spec workflow", () => {
    expect(BASE_SYSTEM_PROMPT).toContain(SPEC_MODE_SYSTEM_PROMPT);
    expect(SPEC_MODE_SYSTEM_PROMPT).toContain("call spec_read");
    expect(SPEC_MODE_SYSTEM_PROMPT).toContain("Never edit it");
    expect(SPEC_MODE_SYSTEM_PROMPT).toContain("spec_* tools");
    expect(SPEC_MODE_SYSTEM_PROMPT).not.toContain("Claude");
    expect(SPEC_MODE_SYSTEM_PROMPT).not.toContain("Codex");
  });
});
