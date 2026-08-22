import { describe, expect, test } from "vitest";

import {
  parseSelectionActionPrompt,
  selectionActionPrompt,
  selectionTurnMarkdown,
} from "./selection-prompt";

const span = {
  specId: "spec-1",
  sectionId: "section-1",
  revision: "56",
  startAnchor: "yjs-section://section-1/00a1bedfb2010100",
  endAnchor: "yjs-section://section-1/02a1bedfb2010000",
  selectedText: "The orchestrator renders each spec section from its stored source.",
  sliceFingerprint: "b115155dd60c95af34e990ae4f06fc846b4c518d0177fab94306dc69cc3067da",
};

const payload = (action: "ask" | "cut" | "custom", instruction: string) => ({
  specId: "spec-1",
  action,
  instruction,
  span,
});

describe("selection prompts", () => {
  // The builder and the parser share one format. A change to either that is
  // not made to the other lands here rather than in the reader's transcript.
  test.each(["ask", "cut", "custom"] as const)("round-trips a %s action", (action) => {
    const parsed = parseSelectionActionPrompt(
      selectionActionPrompt(payload(action, "Tighten this to one sentence.")),
    );

    expect(parsed).toEqual({
      action,
      instruction: "Tighten this to one sentence.",
      sectionId: "section-1",
      selectedText: span.selectedText,
    });
  });

  test("leaves an ordinary turn alone", () => {
    expect(parseSelectionActionPrompt("Scope it to the orchestrator, please.")).toBeNull();
  });

  // Text that merely mentions the marker must not be mistaken for a payload.
  test("returns null when the marker carries no valid JSON", () => {
    expect(parseSelectionActionPrompt("Selection JSON: none of it parsed")).toBeNull();
  });

  test("keeps the anchors and the fingerprint out of what the reader sees", () => {
    const parsed = parseSelectionActionPrompt(selectionActionPrompt(payload("ask", "")))!;

    const shown = selectionTurnMarkdown(parsed, "Problem");

    expect(shown).toBe(`Asked about a passage in §Problem\n\n> ${span.selectedText}`);
    expect(shown).not.toContain(span.sliceFingerprint);
    expect(shown).not.toContain("yjs-section://");
    expect(shown).not.toContain("selection_revision");
  });

  test("repeats only a custom instruction, which the headline does not carry", () => {
    const preset = parseSelectionActionPrompt(
      selectionActionPrompt(payload("cut", "Remove this passage.")),
    )!;
    const custom = parseSelectionActionPrompt(
      selectionActionPrompt(payload("custom", "Name the file it refers to.")),
    )!;

    expect(selectionTurnMarkdown(preset, "Design")).toBe(
      `Asked to cut a passage from §Design\n\n> ${span.selectedText}`,
    );
    expect(selectionTurnMarkdown(custom, "Design")).toContain("Name the file it refers to.");
  });

  test("elides a long passage and survives an unknown section", () => {
    const long = { ...span, selectedText: "word ".repeat(200) };
    const parsed = parseSelectionActionPrompt(
      selectionActionPrompt({ specId: "spec-1", action: "ask", instruction: "", span: long }),
    )!;

    const shown = selectionTurnMarkdown(parsed, undefined);

    expect(shown).toContain("a passage in the document");
    expect(shown).toContain("…");
    expect(shown.length).toBeLessThan(400);
  });
});
