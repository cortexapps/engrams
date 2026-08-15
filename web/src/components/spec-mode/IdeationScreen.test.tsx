import { act, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { Awareness, applyAwarenessUpdate, encodeAwarenessUpdate } from "y-protocols/awareness";
import * as Y from "yjs";

import { renderWithProviders } from "@/test-utils";
import { IdeationScreen } from "./IdeationScreen";

const conversation = vi.hoisted(() => ({
  entries: [] as Array<{
    kind: "agent";
    id: string;
    text: string;
    createdAt: string | null;
  }>,
  acknowledgedPromptIds: new Set<string>(),
  isRunning: false,
  toolLabel: null as string | null,
  hasError: false,
}));

vi.mock("./useSpecConversation", () => ({
  useSpecConversation: () => conversation,
}));

vi.mock("@/hooks/useSpecMessages", () => ({
  useSendSpecMessage: () => ({
    mutateAsync: vi.fn(async () => ({ promptId: "prompt-1" })),
  }),
}));

describe("IdeationScreen", () => {
  beforeEach(() => {
    conversation.entries = [];
    conversation.isRunning = false;
    conversation.toolLabel = null;
    conversation.hasError = false;
  });

  it("renders an inviting centered thread instead of the drafting shell", async () => {
    const user = userEvent.setup();
    const onStartDrafting = vi.fn();
    const { container } = renderWithProviders(
      <IdeationScreen
        specId="spec-1"
        title="Quota design"
        templateName="Engineering spec"
        isStartingDrafting={false}
        startDraftingError={null}
        onStartDrafting={onStartDrafting}
      />,
    );

    expect(await screen.findByRole("main", { name: "Spec ideation" })).toBeTruthy();
    expect(screen.getByText("Start with what you need to work through")).toBeTruthy();
    expect(screen.getByText("Nothing is written down yet.")).toBeTruthy();
    expect(screen.getByRole("log", { name: "Spec conversation" })).toBeTruthy();
    expect(screen.queryByRole("region", { name: "Spec document" })).toBeNull();
    expect(getComputedStyle(container.querySelector(".spec-mode-ideation")!).maxWidth).toBe(
      "700px",
    );

    await user.click(screen.getByRole("button", { name: "Start drafting" }));
    expect(onStartDrafting).toHaveBeenCalledTimes(1);
  });

  it("derives the bridge note from an agent finding and keeps its citation visible", async () => {
    conversation.entries = [
      {
        kind: "agent",
        id: "agent-1",
        text: "The limiter uses one bucket per user. `gateway/limits.rs @ 8f2c1a4`",
        createdAt: null,
      },
    ];

    renderWithProviders(
      <IdeationScreen
        specId="spec-1"
        title="Quota design"
        templateName="Engineering spec"
        isStartingDrafting={false}
        startDraftingError={null}
        onStartDrafting={vi.fn()}
      />,
    );

    expect(
      await screen.findByText("The limiter uses one bucket per user.", { exact: false }),
    ).toBeTruthy();
    expect(screen.getByText("gateway/limits.rs @ 8f2c1a4")).toBeTruthy();
    expect(
      screen.getByText("The conversation has findings ready to shape the document."),
    ).toBeTruthy();
    expect(screen.queryByText("Nothing is written down yet.")).toBeNull();
  });

  it("shows a collaborator when they arrive through awareness", async () => {
    const receiverDoc = new Y.Doc();
    const receiver = new Awareness(receiverDoc);
    receiver.setLocalState(null);
    const remoteDoc = new Y.Doc();
    const remote = new Awareness(remoteDoc);
    remote.setLocalState({ user: { id: "priya", name: "Priya", color: "#b85c0a" } });
    const view = renderWithProviders(
      <IdeationScreen
        specId="spec-1"
        title="Quota design"
        templateName="Engineering spec"
        awareness={receiver}
        isStartingDrafting={false}
        startDraftingError={null}
        onStartDrafting={vi.fn()}
      />,
    );

    await screen.findByRole("main", { name: "Spec ideation" });
    act(() => {
      applyAwarenessUpdate(receiver, encodeAwarenessUpdate(remote, [remoteDoc.clientID]), "test");
    });

    expect(await screen.findByText("Priya")).toBeTruthy();
    expect(screen.getByLabelText("2 people present")).toBeTruthy();
    view.unmount();
    remote.destroy();
    remoteDoc.destroy();
    receiver.destroy();
    receiverDoc.destroy();
  });

  it("shows a bridge failure in ideation", async () => {
    renderWithProviders(
      <IdeationScreen
        specId="spec-1"
        title="Quota design"
        templateName="Engineering spec"
        isStartingDrafting={false}
        startDraftingError="Drafting did not start: control plane unavailable. You are still in the conversation."
        onStartDrafting={vi.fn()}
      />,
    );

    expect((await screen.findByRole("alert")).textContent).toContain(
      "You are still in the conversation.",
    );
    expect(screen.getByRole("main", { name: "Spec ideation" })).toBeTruthy();
  });
});
