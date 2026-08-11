import { describe, expect, test } from "bun:test";

import { BASE_SYSTEM_PROMPT, systemPromptForTaskType } from "../base.ts";
import { SPEC_MODE_SYSTEM_PROMPT, type SpecPromptContext } from "../spec-mode.ts";

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

  test("a spec task without a template keeps the standing rules alone", () => {
    const prompt = systemPromptForTaskType("spec");
    expect(prompt).toBe(`${BASE_SYSTEM_PROMPT}\n\n${SPEC_MODE_SYSTEM_PROMPT}`);
  });

  test("a template shapes the spec prompt, and only for spec tasks", () => {
    const spec: SpecPromptContext = {
      layers: [{ key: "intent", title: "Intent" }],
      sections: [
        {
          key: "problem",
          title: "Problem",
          layerKey: "intent",
          guidance: "State the user problem.",
          doneCriteria: [],
          required: true,
          allowNa: false,
        },
      ],
      stageFlags: { alternatives: "on", talkItThrough: "on", gapCheck: "on" },
    };

    expect(systemPromptForTaskType("spec", spec)).toContain("State the user problem.");
    // A template never leaks into another task type.
    expect(systemPromptForTaskType("chat", spec)).toBe(BASE_SYSTEM_PROMPT);
  });
});
