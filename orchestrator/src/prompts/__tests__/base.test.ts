import { describe, expect, test } from "bun:test";

import {
  BASE_SYSTEM_PROMPT,
  SPEC_MODE_SYSTEM_PROMPT,
  systemPromptForTaskType,
} from "../base.ts";

describe("base system prompt", () => {
  test("adds the live spec workflow only to spec tasks", () => {
    expect(BASE_SYSTEM_PROMPT).not.toContain(SPEC_MODE_SYSTEM_PROMPT);
    expect(systemPromptForTaskType()).toBe(BASE_SYSTEM_PROMPT);
    expect(systemPromptForTaskType("chat")).toBe(BASE_SYSTEM_PROMPT);
    expect(systemPromptForTaskType("spec")).toContain(SPEC_MODE_SYSTEM_PROMPT);
    expect(SPEC_MODE_SYSTEM_PROMPT).toContain("call spec_read");
    expect(SPEC_MODE_SYSTEM_PROMPT).toContain("Never edit it");
    expect(SPEC_MODE_SYSTEM_PROMPT).toContain("spec_* tools");
    expect(SPEC_MODE_SYSTEM_PROMPT).not.toContain("Claude");
    expect(SPEC_MODE_SYSTEM_PROMPT).not.toContain("Codex");
  });
});
