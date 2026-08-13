import { parseMarkdown, SPEC_FRAGMENT_NAME } from "@engrams/spec-document";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import { describe, expect, test } from "vitest";
import * as Y from "yjs";

import type { SpecRail } from "@/hooks/useSpecRead";
import { deriveNextProposal, deriveSpecSurface } from "./spec-surface";

describe("deriveSpecSurface", () => {
  test("keeps rail state while deriving invitations and open questions from the document", () => {
    const document = yDocument(
      "## Problem\n\n\n\n## API\n\nA route. {{open-question:q1}} {{open-question:q2}}\n",
    );
    const surface = deriveSpecSurface(rail(), document);

    expect(surface.sections).toMatchObject([
      { id: "problem", state: "settled", isEmpty: true, openQuestionCount: 0 },
      { id: "api", state: "open", isEmpty: false, openQuestionCount: 2 },
    ]);
    document.destroy();
  });

  test("uses rail question counts before the document loads and derives credit without formatting copy", () => {
    const surface = deriveSpecSurface(rail(), null);

    expect(surface.sections[0]).toMatchObject({
      state: "settled",
      isEmpty: false,
      openQuestionCount: 3,
      credit: {
        by: { id: "user-1", name: "Nikhil" },
        at: "2026-08-13T10:00:00.000Z",
      },
    });
  });
});

describe("deriveNextProposal", () => {
  test("offers the first open section before a later proposed section", () => {
    const surface = deriveSpecSurface(rail(), null);
    expect(deriveNextProposal(surface)).toEqual({
      kind: "draft_section",
      sectionId: "api",
      sectionTitle: "API",
    });
  });

  test("offers settlement when no section is open", () => {
    const surface = deriveSpecSurface(
      rail([
        { id: "problem", state: "settled" },
        { id: "api", state: "proposed" },
      ]),
      null,
    );
    expect(deriveNextProposal(surface)).toEqual({
      kind: "settle_section",
      sectionId: "api",
      sectionTitle: "API",
    });
  });

  test("offers breakage inspection after every section is settled", () => {
    const surface = deriveSpecSurface(
      rail([
        { id: "problem", state: "settled" },
        { id: "api", state: "settled" },
      ]),
      null,
    );
    expect(deriveNextProposal(surface)).toEqual({ kind: "look_for_breakage" });
  });
});

function yDocument(markdown: string): Y.Doc {
  const document = new Y.Doc();
  prosemirrorToYXmlFragment(
    parseMarkdown(markdown, {
      sections: [
        { id: "problem", key: "problem", title: "Problem" },
        { id: "api", key: "api", title: "API" },
      ],
    }),
    document.getXmlFragment(SPEC_FRAGMENT_NAME),
  );
  return document;
}

function rail(
  states: Array<{ id: "problem" | "api"; state: "open" | "proposed" | "settled" | "n/a" }> = [
    { id: "problem", state: "settled" },
    { id: "api", state: "open" },
  ],
): SpecRail {
  return {
    sections: states.map(({ id, state }) => ({
      id,
      templateKey: id,
      title: id === "problem" ? "Problem" : "API",
      state,
      naReason: state === "n/a" ? "Not this spec" : null,
      allowNa: true,
      openQuestionCount: id === "problem" ? 3 : 4,
      settledBy: id === "problem" ? { id: "user-1", name: "Nikhil" } : null,
      stateChangedAt: id === "problem" ? "2026-08-13T10:00:00.000Z" : null,
    })),
    completeness: {
      complete: states.filter(({ state }) => state === "settled" || state === "n/a").length,
      total: states.length,
    },
  };
}
