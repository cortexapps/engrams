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

  test("keeps document sections without the deleted process machinery", () => {
    const prompt = specModeSystemPrompt(context());

    expect(prompt).toContain("- Alternatives (layer: System; required) — Compare the serious");
    expect(prompt).toContain("### The structure of this spec");
    expect(prompt).not.toMatch(/\bstage\b/i);
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

    expect(prompt).toContain("- Problem (layer: Intent; required) — State the problem.");
    expect(prompt).not.toContain("Done when:");
  });
});
