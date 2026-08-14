import { parseMarkdown, SPEC_FRAGMENT_NAME } from "@engrams/spec-document";
import { prosemirrorToYXmlFragment, yXmlFragmentToProseMirrorRootNode } from "y-prosemirror";
import { Awareness, applyAwarenessUpdate, encodeAwarenessUpdate } from "y-protocols/awareness";
import { describe, expect, test } from "vitest";
import * as Y from "yjs";
import { schema } from "@engrams/spec-document";

import { collaboratorColor, MISSING_COLLABORATOR_COLOR } from "./collaborator-colors";
import {
  mapAwarenessCursorsToSections,
  readSpecPresence,
  sectionIdAtPos,
} from "./section-presence";

describe("section presence", () => {
  test("maps ProseMirror positions to top-level section ids", () => {
    const { document, ydoc } = specDocument();
    expect(sectionIdAtPos(document, 2)).toBe("problem");
    expect(sectionIdAtPos(document, document.content.size)).toBeNull();
    expect(sectionIdAtPos(document, -1)).toBeNull();
    ydoc.destroy();
  });

  test("maps awareness cursors through the editor-owned position resolver", () => {
    const { document, ydoc } = specDocument();
    const receiver = new Awareness(new Y.Doc());
    receiver.setLocalState(null);
    const alice = awarenessState({
      user: { id: "alice", name: "Alice", color: "#2563eb" },
      cursor: { anchor: "a", head: "h" },
    });
    applyAwarenessUpdate(receiver, alice.update, "test");

    expect(mapAwarenessCursorsToSections(receiver, document, () => 2)).toEqual([
      {
        clientId: expect.any(Number),
        userId: "alice",
        name: "Alice",
        color: collaboratorColor("alice"),
        sectionId: "problem",
      },
    ]);

    alice.destroy();
    receiver.destroy();
    ydoc.destroy();
  });

  test("reads human and synthesized agent entries from one reader", () => {
    const receiver = new Awareness(new Y.Doc());
    receiver.setLocalState(null);
    const state = awarenessState({
      user: { id: "alice", name: "Alice", color: "#2563eb" },
      location: { sectionId: "problem" },
      agentPresence: [
        {
          name: "engram",
          sessionId: "session-1",
          toolCallId: "tool-1",
          sectionId: "failure-modes",
        },
      ],
    });
    applyAwarenessUpdate(receiver, state.update, "test");

    expect(readSpecPresence(receiver)).toMatchObject([
      {
        kind: "human",
        id: "alice",
        name: "Alice",
        color: collaboratorColor("alice"),
        sectionId: "problem",
        isSelf: false,
      },
      { kind: "agent", name: "engram", sectionId: "failure-modes" },
    ]);

    state.destroy();
    receiver.destroy();
  });

  test("uses the shared neutral color when a person has no id", () => {
    const receiver = new Awareness(new Y.Doc());
    receiver.setLocalState({ user: { name: "Former member", color: "#ffffff" } });

    expect(readSpecPresence(receiver)).toMatchObject([
      { kind: "human", name: "Former member", color: MISSING_COLLABORATOR_COLOR },
    ]);

    receiver.destroy();
  });
});

function specDocument() {
  const ydoc = new Y.Doc();
  prosemirrorToYXmlFragment(
    parseMarkdown("## Problem\n\nText.\n\n## API\n\nRoute.\n", {
      sections: [
        { id: "problem", key: "problem", title: "Problem" },
        { id: "api", key: "api", title: "API" },
      ],
    }),
    ydoc.getXmlFragment(SPEC_FRAGMENT_NAME),
  );
  return {
    ydoc,
    document: yXmlFragmentToProseMirrorRootNode(ydoc.getXmlFragment(SPEC_FRAGMENT_NAME), schema),
  };
}

function awarenessState(state: Record<string, unknown>): {
  update: Uint8Array;
  destroy(): void;
} {
  const doc = new Y.Doc();
  const awareness = new Awareness(doc);
  awareness.setLocalState(state);
  return {
    update: encodeAwarenessUpdate(awareness, [doc.clientID]),
    destroy: () => awareness.destroy(),
  };
}
