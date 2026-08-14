import { act, render, screen, waitFor } from "@testing-library/react";
import {
  Awareness,
  applyAwarenessUpdate,
  encodeAwarenessUpdate,
  removeAwarenessStates,
} from "y-protocols/awareness";
import { afterEach, describe, expect, test } from "vitest";
import * as Y from "yjs";

import { collaboratorColor } from "./collaborator-colors";
import { PresenceGroup } from "./PresenceGroup";
import { SectionList } from "./SectionList";
import { InlineSectionPresence } from "./SectionNodeView";
import { useSpecPresence } from "./section-presence";
import type { SpecSurface, SpecSurfaceSection } from "./spec-surface";

const documents: Y.Doc[] = [];
const awarenessStores: Awareness[] = [];

afterEach(() => {
  for (const awareness of awarenessStores.splice(0)) awareness.destroy();
  for (const document of documents.splice(0)) document.destroy();
});

describe("presence surfaces", () => {
  test("renders two people by section with one stable color and keeps the agent in the top bar", () => {
    const receiver = awareness();
    receiver.setLocalState({
      user: { id: "current-user", name: "Current User", color: "#000000" },
      location: { sectionId: "problem" },
    });
    const alice = awarenessState({
      user: { id: "alice", name: "Alice", color: "#ffffff" },
      location: { sectionId: "design" },
    });
    const agent = awarenessState({
      agentPresence: [
        {
          name: "engram",
          sessionId: "session-1",
          toolCallId: "tool-1",
          sectionId: "problem",
        },
      ],
    });
    applyAwarenessUpdate(receiver, alice.update, "test");
    applyAwarenessUpdate(receiver, agent.update, "test");

    const view = render(<PresenceHarness awareness={receiver} />);

    expect(screen.getByText("you", { selector: ".spec-mode-presence-label" })).toBeTruthy();
    expect(screen.getAllByText("Alice")).toHaveLength(2);
    expect(screen.getByText("engram · §Problem")).toBeTruthy();
    expect(view.container.querySelectorAll(".spec-mode-section-presence-dot")).toHaveLength(2);
    expect(view.container.querySelectorAll(".spec-mode-inline-cursor")).toHaveLength(2);
    const agentAvatar = view.container.querySelector(
      '[data-presence-kind="agent"] .spec-mode-presence-avatar',
    );
    expect(agentAvatar?.className).toContain("is-agent");

    const aliceColor = collaboratorColor("alice");
    const alicePair = screen.getByText("Alice", {
      selector: ".spec-mode-presence-label",
    }).parentElement;
    const aliceAvatar = alicePair?.querySelector<HTMLElement>(".spec-mode-presence-avatar");
    const aliceDot = screen.getByLabelText("Alice is in Proposed design");
    const aliceFlag = screen.getByLabelText("Alice is in this section");
    const aliceChip = aliceFlag.querySelector<HTMLElement>(".spec-mode-inline-cursor-name");
    expect(aliceAvatar?.style.backgroundColor).toBe(aliceDot.style.backgroundColor);
    expect(aliceChip?.style.backgroundColor).toBe(aliceDot.style.backgroundColor);
    expect(aliceColor).not.toBe("#ffffff");
    const colorProbe = document.createElement("span");
    colorProbe.style.backgroundColor = aliceColor;
    expect(aliceAvatar?.style.backgroundColor).toBe(colorProbe.style.backgroundColor);

    alice.destroy();
    agent.destroy();
  });

  test("renders no presence chrome when awareness is empty", () => {
    const receiver = awareness();
    receiver.setLocalState(null);
    const view = render(<PresenceHarness awareness={receiver} />);

    expect(view.container.querySelector(".spec-mode-presence-group")).toBeNull();
    expect(view.container.querySelector(".spec-mode-section-presence-dots")).toBeNull();
    expect(view.container.querySelector(".spec-mode-inline-cursors")).toBeNull();
  });

  test("removes a departed person's stale awareness from every surface", async () => {
    const receiver = awareness();
    receiver.setLocalState(null);
    const alice = awarenessState({
      user: { id: "alice", name: "Alice" },
      location: { sectionId: "design" },
    });
    applyAwarenessUpdate(receiver, alice.update, "test");
    render(<PresenceHarness awareness={receiver} />);
    expect(screen.getAllByText("Alice")).toHaveLength(2);

    act(() => removeAwarenessStates(receiver, [alice.clientId], "test"));

    await waitFor(() => expect(screen.queryByText("Alice")).toBeNull());
    expect(screen.queryByLabelText("Alice is in Proposed design")).toBeNull();
    expect(screen.queryByLabelText("Alice is in this section")).toBeNull();
    alice.destroy();
  });
});

function PresenceHarness({ awareness }: { awareness: Awareness }) {
  const presence = useSpecPresence(awareness);
  const value = surface();
  return (
    <>
      <PresenceGroup presence={presence} sections={value.sections} />
      <SectionList surface={value} presence={presence} onSelectSection={() => undefined} />
      {value.sections.map((section) => (
        <InlineSectionPresence key={section.id} presence={presence} sectionId={section.id} />
      ))}
    </>
  );
}

function awareness(): Awareness {
  const document = new Y.Doc();
  const value = new Awareness(document);
  documents.push(document);
  awarenessStores.push(value);
  return value;
}

function awarenessState(state: Record<string, unknown>): {
  clientId: number;
  update: Uint8Array;
  destroy(): void;
} {
  const document = new Y.Doc();
  const value = new Awareness(document);
  value.setLocalState(state);
  return {
    clientId: document.clientID,
    update: encodeAwarenessUpdate(value, [document.clientID]),
    destroy: () => {
      value.destroy();
      document.destroy();
    },
  };
}

function surface(): SpecSurface {
  return {
    sections: [
      section({ id: "problem", title: "Problem" }),
      section({ id: "design", title: "Proposed design" }),
    ],
    settledCount: 0,
    totalCount: 2,
    openQuestions: [],
    provenanceRanges: [],
    next: null,
  };
}

function section(overrides: Partial<SpecSurfaceSection>): SpecSurfaceSection {
  const id = overrides.id ?? "section";
  return {
    id,
    templateKey: id,
    title: overrides.title ?? "Section",
    state: "open",
    allowNa: true,
    naReason: null,
    isEmpty: false,
    isReached: true,
    isBeingRead: false,
    openQuestionCount: 0,
    settledBy: null,
    stateChangedAt: null,
    credit: null,
    provenance: [],
    ...overrides,
  };
}
