import { render, screen } from "@testing-library/react";
import { describe, expect, test } from "vitest";
import { Awareness, applyAwarenessUpdate, encodeAwarenessUpdate } from "y-protocols/awareness";
import * as Y from "yjs";

import { readSpecPresence } from "@/components/spec-mode/section-presence";
import { SpecPresence } from "./SpecPresence";

describe("spec presence", () => {
  test("renders three participants, including section-level agent presence", () => {
    const receiver = new Awareness(new Y.Doc());
    receiver.setLocalState(null);
    const alice = awarenessState({ user: { id: "alice", name: "Alice", color: "#2563eb" } });
    const bob = awarenessState({ user: { id: "bob", name: "Bob", color: "#7c3aed" } });
    const agent = awarenessState({
      agentPresence: [
        {
          name: "engram",
          sessionId: "session-1",
          toolCallId: "tool-1",
          sectionId: "Failure modes",
        },
      ],
    });
    applyAwarenessUpdate(receiver, alice.update, "test");
    applyAwarenessUpdate(receiver, bob.update, "test");
    applyAwarenessUpdate(receiver, agent.update, "test");

    expect(readSpecPresence(receiver)).toHaveLength(3);
    render(<SpecPresence awareness={receiver} />);
    expect(screen.getByLabelText("3 participants present")).toBeTruthy();
    expect(screen.getByText("Alice")).toBeTruthy();
    expect(screen.getByText("Bob")).toBeTruthy();
    expect(screen.getByText("engram ✎ §Failure modes")).toBeTruthy();

    alice.destroy();
    bob.destroy();
    agent.destroy();
    receiver.destroy();
  });
});

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
