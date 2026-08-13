import { describe, expect, test } from "bun:test";

import type { SpecTemplateStageFlags, SpecTemplateStageMode } from "../../db/schema.ts";
import { ENGINEERING_DESIGN_TEMPLATE } from "../../specs/template-catalog.ts";
import { SPEC_MODE_SYSTEM_PROMPT, specModeSystemPrompt } from "../spec-mode.ts";

/** The built-in template, with the stage flags that a test needs. */
function context(flags: Partial<SpecTemplateStageFlags> = {}) {
  return {
    layers: ENGINEERING_DESIGN_TEMPLATE.layers,
    sections: ENGINEERING_DESIGN_TEMPLATE.sections,
    stageFlags: {
      ...ENGINEERING_DESIGN_TEMPLATE.stageFlags,
      ...flags,
    },
  };
}

const ALTERNATIVES_STAGE_HEADING = "### Alternatives stage";
const ALTERNATIVES_STAGE_BODY = "at least two credible directions";
const TALK_IT_THROUGH_STAGE_HEADING = "### Talk it through stage";
const GAP_CHECK_STAGE_HEADING = "### Gap check stage";

describe("spec mode prompt", () => {
  test("keeps the standing rules with no template", () => {
    expect(specModeSystemPrompt()).toBe(SPEC_MODE_SYSTEM_PROMPT);
  });

  test("states the recon and provenance rules", () => {
    const prompt = specModeSystemPrompt(context());

    // Recon first (R7): repository first, document content first, questions last.
    expect(prompt).toContain("Read the repository before you write your first message.");
    expect(prompt).toContain("Never open with a list of questions.");
    expect(prompt).toContain("spec_update_section");
    // Provenance (R16).
    expect(prompt).toContain("path/to/file.ts @ 8f2c1a4");
    expect(prompt).toContain('write "unverified" next to it');
    expect(prompt).not.toContain("frontier");
    expect(prompt).not.toContain("provisional");
  });

  test("carries the template's layers, section guidance and done criteria", () => {
    const prompt = specModeSystemPrompt(context());

    expect(prompt).toContain("1. Intent — Define the problem, the desired outcome");
    expect(prompt).toContain("3. System — Define the implementation");
    expect(prompt).toContain(
      "- Problem (layer: Intent; required) — State the user or system problem",
    );
    expect(prompt).toContain("Done when: The affected user or system is clear.");
    // An optional section states that it is optional, and whether n/a is allowed.
    expect(prompt).toContain("- API (layer: Contract; optional; n/a is permitted with a reason)");
  });

  test("a stage that is off contributes no instruction", () => {
    const on = specModeSystemPrompt(context({ alternatives: "on" }));
    const off = specModeSystemPrompt(context({ alternatives: "off" }));

    expect(on).toContain(ALTERNATIVES_STAGE_HEADING);
    expect(on).toContain(ALTERNATIVES_STAGE_BODY);
    expect(off).not.toContain(ALTERNATIVES_STAGE_HEADING);
    expect(off).not.toContain(ALTERNATIVES_STAGE_BODY);
    // A stage is process and a section is structure: the flag removes the
    // stage, and it never removes the section of the same name.
    expect(off).toContain("- Alternatives (layer: System; required) — Compare the serious");
    // The other stages are untouched.
    expect(off).toContain(TALK_IT_THROUGH_STAGE_HEADING);
    expect(off).toContain(GAP_CHECK_STAGE_HEADING);
  });

  test("every stage disappears when the template turns all of them off", () => {
    const allOff: SpecTemplateStageFlags = {
      alternatives: "off",
      talkItThrough: "off",
      gapCheck: "off",
    };
    const prompt = specModeSystemPrompt(context(allOff));

    for (const heading of [
      ALTERNATIVES_STAGE_HEADING,
      TALK_IT_THROUGH_STAGE_HEADING,
      GAP_CHECK_STAGE_HEADING,
    ]) {
      expect(prompt).not.toContain(heading);
    }
    expect(prompt).toContain("### The structure of this spec");
  });

  test("on runs a stage and suggested offers it", () => {
    const on = specModeSystemPrompt(context({ talkItThrough: "on" }));
    const suggested = specModeSystemPrompt(context({ talkItThrough: "suggested" }));

    expect(on).toContain("This spec uses the talk-it-through stage.");
    expect(on).not.toContain("This spec offers the talk-it-through stage.");
    expect(suggested).toContain("This spec offers the talk-it-through stage.");
    expect(suggested).toContain("start it only when they accept");
    expect(suggested).not.toContain("This spec uses the talk-it-through stage.");
    // Both modes carry the same description of the stage.
    const body = "you keep the working notes with spec_update_notes";
    expect(on).toContain(body);
    expect(suggested).toContain(body);
  });

  test("the assembled prompt names no harness", () => {
    for (const mode of ["on", "suggested", "off"] as SpecTemplateStageMode[]) {
      const prompt = specModeSystemPrompt(
        context({ alternatives: mode, talkItThrough: mode, gapCheck: mode }),
      );
      expect(prompt).not.toContain("Claude");
      expect(prompt).not.toContain("Codex");
    }
  });

  test("a section without done criteria omits the line", () => {
    const prompt = specModeSystemPrompt({
      layers: [{ key: "intent", title: "Intent" }],
      sections: [
        {
          key: "problem",
          title: "Problem",
          layerKey: "intent",
          guidance: "State the problem.",
          doneCriteria: [],
          required: true,
          allowNa: false,
        },
      ],
      stageFlags: { alternatives: "off", talkItThrough: "off", gapCheck: "off" },
    });

    expect(prompt).toContain("- Problem (layer: Intent; required) — State the problem.");
    expect(prompt).not.toContain("Done when:");
  });
});
