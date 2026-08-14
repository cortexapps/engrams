import { parseMarkdown, SPEC_FRAGMENT_NAME } from "@engrams/spec-document";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import * as Y from "yjs";

import type { SpecRail } from "@/hooks/useSpecRead";
import {
  deriveNextProposal,
  deriveSpecSurface,
  isSectionComplete,
  useSpecSurface,
} from "./spec-surface";

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
    expect(surface.openQuestions).toEqual([
      { id: "q1", resolved: false, text: null },
      { id: "q2", resolved: false, text: null },
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

  test("marks only the first empty open section as reached", () => {
    const document = yDocument("## Problem\n\n\n\n## API\n\n");
    const surface = deriveSpecSurface(
      rail([
        { id: "problem", state: "open" },
        { id: "api", state: "open" },
      ]),
      document,
    );

    expect(surface.sections.map(({ id, isReached }) => ({ id, isReached }))).toEqual([
      { id: "problem", isReached: true },
      { id: "api", isReached: false },
    ]);
    document.destroy();
  });

  test("derives provenance labels and decoration ranges once", () => {
    const document = yDocument(
      "## Problem\n\nThe limiter is in gateway/limits.rs @ 8f2c1a4.\n\n## API\n\nA route.\n",
    );
    const surface = deriveSpecSurface(rail(), document);

    expect(surface.sections[0]?.provenance).toEqual([{ label: "gateway/limits.rs @ 8f2c1a4" }]);
    expect(surface.provenanceRanges).toEqual([
      expect.objectContaining({ label: "gateway/limits.rs @ 8f2c1a4" }),
    ]);
    document.destroy();
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

  test("offers breakage inspection when settled and n/a sections complete the spec", () => {
    const surface = deriveSpecSurface(
      rail([
        { id: "problem", state: "settled" },
        { id: "api", state: "n/a" },
      ]),
      null,
    );
    expect(deriveNextProposal(surface)).toEqual({ kind: "look_for_breakage" });
  });

  test("offers breakage inspection when every section is n/a", () => {
    const surface = deriveSpecSurface(
      rail([
        { id: "problem", state: "n/a" },
        { id: "api", state: "n/a" },
      ]),
      null,
    );
    expect(deriveNextProposal(surface)).toEqual({ kind: "look_for_breakage" });
  });

  test("offers no next step when the spec has no sections", () => {
    const surface = deriveSpecSurface(rail([]), null);
    expect(deriveNextProposal(surface)).toBeNull();
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

describe("useSpecSurface", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  test("a burst of document updates costs one derivation, not one per update", () => {
    // Deriving rebuilds the whole document and scans every text node, and the
    // editor turns each keystroke into a Yjs update. Typing must not pay that
    // per character.
    const doc = yDocument("## Problem\n\nThe limiter retries forever.\n\n## API\n\nOne route.\n");
    const railValue = rail();
    const { result } = renderHook(() => useSpecSurface(railValue, doc));
    const first = result.current;

    act(() => {
      for (let i = 0; i < 25; i += 1) {
        doc.getMap("burst").set(`key-${i}`, i);
      }
    });
    // Nothing settled yet, so the surface identity is unchanged.
    expect(result.current).toBe(first);

    act(() => void vi.advanceTimersByTime(200));
    const afterBurst = result.current;
    expect(afterBurst).not.toBe(first);

    // A quiet period does not derive again.
    act(() => void vi.advanceTimersByTime(1_000));
    expect(result.current).toBe(afterBurst);
  });
});

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
      complete: states.filter(isSectionComplete).length,
      total: states.length,
    },
  };
}
