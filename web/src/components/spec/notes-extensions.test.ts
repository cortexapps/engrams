import { describe, expect, test } from "vitest";
import {
  buildWorkingNotesDocument,
  notesSchema,
  readWorkingNotes,
  untaggedBulletCount,
  type SpecWorkingNotes,
} from "@engrams/spec-document";
import { EditorState } from "@tiptap/pm/state";

import { createNotesStructurePlugin, notesStructure } from "./notes-extensions";

function notes(): SpecWorkingNotes {
  return {
    clusters: [
      {
        id: "burst",
        theme: "burst semantics",
        sectionIds: ["behavior"],
        bullets: [
          {
            id: "b1",
            mark: "verified",
            kind: "observation",
            text: "orgs get a burst credit pool, refills daily",
            provenance: "your call, firm",
            agentText: "orgs get a burst credit pool, refills daily",
          },
          {
            id: "b2",
            mark: "contradicted",
            kind: "observation",
            text: "the limiter already handles bursts",
            provenance: "limits.rs @ 8f2c1a4: a per-user bucket",
            agentText: "the limiter already handles bursts",
          },
        ],
      },
      {
        id: "pile",
        theme: "untagged",
        sectionIds: [],
        bullets: [
          {
            id: "b3",
            mark: "unchecked",
            kind: "question",
            text: "how fine is the billing granularity?",
            provenance: null,
            agentText: "how fine is the billing granularity?",
          },
        ],
      },
    ],
  };
}

function editorState() {
  return EditorState.create({
    doc: buildWorkingNotesDocument(notes()),
    plugins: [createNotesStructurePlugin()],
  });
}

/** The first text position inside the first bullet. */
const FIRST_BULLET_TEXT = 2;

describe("the working-notes editor", () => {
  test("a person rewrites the words in a bullet", () => {
    const state = editorState();

    const result = state.applyTransaction(state.tr.insertText("weekly: ", FIRST_BULLET_TEXT));

    expect(result.transactions).toHaveLength(1);
    const after = readWorkingNotes(result.state.doc);
    expect(after.clusters[0]!.bullets[0]!.text).toBe(
      "weekly: orgs get a burst credit pool, refills daily",
    );
    // The agent's own wording is untouched, so the correction is reportable.
    expect(after.clusters[0]!.bullets[0]!.agentText).toBe(
      "orgs get a burst credit pool, refills daily",
    );
  });

  test("a person cannot split a bullet in two", () => {
    const state = editorState();

    const result = state.applyTransaction(state.tr.split(FIRST_BULLET_TEXT + 4));

    expect(result.transactions).toHaveLength(0);
    expect(readWorkingNotes(result.state.doc).clusters[0]!.bullets).toHaveLength(2);
  });

  test("a person cannot delete a bullet", () => {
    const state = editorState();
    const cluster = state.doc.child(0);
    const bullet = cluster.child(0);

    const result = state.applyTransaction(state.tr.delete(1, 1 + bullet.nodeSize));

    expect(result.transactions).toHaveLength(0);
    expect(readWorkingNotes(result.state.doc).clusters[0]!.bullets).toHaveLength(2);
  });

  test("a person cannot change a mark or a destination tag", () => {
    const state = editorState();
    const bullet = state.doc.child(0).child(0);

    const remark = state.applyTransaction(
      state.tr.setNodeMarkup(1, undefined, { ...bullet.attrs, mark: "unchecked" }),
    );
    const retag = state.applyTransaction(
      state.tr.setNodeMarkup(0, undefined, { ...state.doc.child(0).attrs, sectionIds: ["api"] }),
    );

    expect(remark.transactions).toHaveLength(0);
    expect(retag.transactions).toHaveLength(0);
  });

  test("the structure fingerprint ignores the words and holds the model", () => {
    const state = editorState();
    const edited = state.applyTransaction(state.tr.insertText("no: ", FIRST_BULLET_TEXT));

    expect(notesStructure(edited.state.doc)).toBe(notesStructure(state.doc));
    // A placeholder document is never mistaken for the server's notes.
    expect(notesStructure(notesSchema.nodes.doc!.create())).not.toBe(notesStructure(state.doc));
  });

  test("the untagged pile counts the bullets with no destination", () => {
    expect(untaggedBulletCount(readWorkingNotes(buildWorkingNotesDocument(notes())))).toBe(1);
  });
});
