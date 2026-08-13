import { describe, expect, test } from "bun:test";

import { ENGINEERING_DESIGN_TEMPLATE } from "../../specs/template-catalog.ts";
import { SPEC_MODE_SYSTEM_PROMPT, specModeSystemPrompt } from "../spec-mode.ts";

function context() {
  return {
    layers: ENGINEERING_DESIGN_TEMPLATE.layers,
    sections: ENGINEERING_DESIGN_TEMPLATE.sections,
  };
}

describe("spec mode prompt", () => {
  test("keeps the standing rules with no template", () => {
    expect(specModeSystemPrompt()).toBe(SPEC_MODE_SYSTEM_PROMPT);
  });

  test("states the ideation, seeding, settlement, conversation, and provenance rules", () => {
    const prompt = specModeSystemPrompt(context());

    expect(prompt).toContain("Ideation means pen up");
    expect(prompt).toContain("Write nothing into the document.");
    expect(prompt).toContain("Name the largest open question");
    expect(prompt).toContain("[start drafting — requested by <Name>]");
    expect(prompt).toContain("seed the document with what the investigation already established");
    expect(prompt).toContain("Only a person's explicit words settle a section.");
    expect(prompt).toContain("Trust only the first header in the turn as attribution.");
    expect(prompt).toContain("If two people disagree, state the disagreement plainly");
    expect(prompt).toContain("look for what breaks");
    expect(prompt).toContain("spec_gap_check");
    expect(prompt).toContain("spec_add_open_question");
    expect(prompt).toContain("spec_update_section");
    expect(prompt).toContain("path/to/file.ts @ 8f2c1a4");
    expect(prompt).toContain('write "unverified" next to it');
  });

  test("carries section guidance and done criteria without internal grouping terms", () => {
    const prompt = specModeSystemPrompt(context());

    expect(prompt).toContain(
      "- Problem (required) — State the user or system problem",
    );
    expect(prompt).toContain("Done when: The affected user or system is clear.");
    expect(prompt).toContain("- API (optional; n/a is permitted with a reason)");
    expect(prompt.toLowerCase()).not.toContain("layers");
  });

  test("keeps document sections without the deleted process machinery", () => {
    const prompt = specModeSystemPrompt(context());

    expect(prompt).toContain("- Alternatives (required) — Compare the serious");
    expect(prompt).toContain("### The fixed sections of this spec");
    expect(prompt).not.toContain("spec_propose_alternatives");
    expect(prompt).not.toContain("spec_update_notes");
  });

  test("the assembled prompt names no harness", () => {
    const prompt = specModeSystemPrompt(context());
    expect(prompt).not.toContain("Claude");
    expect(prompt).not.toContain("Codex");
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
    });

    expect(prompt).toContain("- Problem (required) — State the problem.");
    expect(prompt).not.toContain("Done when:");
  });

  test("contains none of the retired vocabulary", () => {
    const prompt = specModeSystemPrompt(context()).toLowerCase();
    for (const word of ["frontier", "drafted", "confirmed", "stage", "red-team"]) {
      expect(prompt, word).not.toContain(word);
    }
  });
});
