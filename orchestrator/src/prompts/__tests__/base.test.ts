import { describe, expect, test } from "bun:test";

import {
  BASE_SYSTEM_PROMPT,
  COORDINATION_SYSTEM_PROMPT,
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

  // The playbook follows the coordination TOOLS, so a session that cannot
  // spawn never reads how to spawn — and the gating can never drift from the
  // manifest it describes.
  test("adds the coordination playbook only when the manifest can spawn", () => {
    expect(BASE_SYSTEM_PROMPT).not.toContain(COORDINATION_SYSTEM_PROMPT);
    expect(systemPromptForTaskType("chat", [])).toBe(BASE_SYSTEM_PROMPT);
    expect(systemPromptForTaskType("chat", ["save_memory"])).toBe(BASE_SYSTEM_PROMPT);
    expect(systemPromptForTaskType("chat", ["save_memory", "spawn_session"])).toContain(
      COORDINATION_SYSTEM_PROMPT,
    );
    // A spec session that can also spawn gets both.
    const both = systemPromptForTaskType("spec", ["spawn_session"]);
    expect(both).toContain(SPEC_MODE_SYSTEM_PROMPT);
    expect(both).toContain(COORDINATION_SYSTEM_PROMPT);
  });

  test("the coordination playbook states the rules no single tool owns", () => {
    expect(COORDINATION_SYSTEM_PROMPT).toContain("one child for each unit of work");
    expect(COORDINATION_SYSTEM_PROMPT).toContain("migration numbers");
    expect(COORDINATION_SYSTEM_PROMPT).toContain("next one in merge order");
    // Harness-agnostic, like every other orchestrator-owned prompt: the
    // sandbox picks the tool spelling, the orchestrator states the policy.
    expect(COORDINATION_SYSTEM_PROMPT).not.toContain("Claude");
    expect(COORDINATION_SYSTEM_PROMPT).not.toContain("Codex");
    expect(COORDINATION_SYSTEM_PROMPT).not.toContain("mcp__engrams__");
  });
});
