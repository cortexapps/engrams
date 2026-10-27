import { describe, expect, it } from "vitest";

import { blockKind, scopeLabel, walkBlocks } from "./automation-blocks";
import { AUTOMATION_TEMPLATES, automationTemplate } from "./automation-templates";

describe("automation templates", () => {
  it("are five distinct starting points, each ending in the Agent block", () => {
    expect(AUTOMATION_TEMPLATES).toHaveLength(5);
    expect(new Set(AUTOMATION_TEMPLATES.map((t) => t.id)).size).toBe(5);
    for (const template of AUTOMATION_TEMPLATES) {
      const blocks = [...walkBlocks(template.definition.blocks)];
      expect(blocks.at(-1)?.type).toBe("run_agent");
      // Every block is one the catalog knows, so the canvas can draw it.
      for (const block of blocks) expect(blockKind(block.type).kind).toBe(block.type);
      // No template names a profile: that is the first thing a person picks.
      expect(blocks.at(-1)?.config["profileId"]).toBe("");
    }
  });

  it("says the scope the list will show", () => {
    for (const template of AUTOMATION_TEMPLATES) {
      expect(scopeLabel(template.definition)).toBe(template.scope);
    }
  });

  it("finds a template by id and nothing by a stale one", () => {
    expect(automationTemplate("pr-reviewer")?.name).toBe("PR reviewer");
    expect(automationTemplate("missing")).toBeUndefined();
    expect(automationTemplate(undefined)).toBeUndefined();
  });
});
