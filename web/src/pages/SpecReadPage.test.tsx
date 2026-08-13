import { createRouterTransport } from "@connectrpc/connect";
import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { SpecSelectionAction, SpecSelectionActionPayload } from "@engrams/spec-document";

import { renderWithProviders } from "@/test-utils";
import { SessionService } from "@/gen/engram/app/v1/session_pb";
import { SpecReadPage } from "./SpecReadPage";

const view: {
  lifecycle: "draft" | "published";
  sessionId: string | null;
  publishedCheckpointId: string | null;
} = {
  lifecycle: "draft",
  sessionId: null,
  publishedCheckpointId: null,
};

const pinned = {
  id: "checkpoint-1",
  label: "Ready to publish",
  authorUserId: "member-1",
  reason: "publish",
  docSeq: "3",
  createdAt: "2026-08-10T01:00:00.000Z",
  markdown: "## Context\n\nPinned content.\n",
  sections: [{ id: "context", title: "Context" }],
};
const older = {
  ...pinned,
  id: "checkpoint-0",
  label: "Initial outline",
  docSeq: "2",
  createdAt: "2026-08-09T01:00:00.000Z",
  markdown: "## Context\n\nInitial content.\n",
};
const restoreMutate = vi.fn();
const readRefetch = vi.fn();

const actionPayload = (action: SpecSelectionAction): SpecSelectionActionPayload => ({
  specId: "spec-1",
  action,
  instruction: action === "custom" ? "Use a bounded retry." : `${action} instruction`,
  span: {
    specId: "spec-1",
    sectionId: "failure-modes",
    revision: "17",
    startAnchor: "start-anchor",
    endAnchor: "end-anchor",
    selectedText: "Retry forever.",
    sliceFingerprint: "a".repeat(64),
  },
});

const rail = {
  completeness: { complete: 1, total: 2 },
  sections: [
    {
      id: "context",
      templateKey: "context",
      title: "Context",
      state: "settled" as const,
      naReason: null,
      allowNa: false,
      openQuestionCount: 0,
      settledBy: { id: "member-1", name: "Ada" },
      stateChangedAt: "2026-08-13T08:00:00.000Z",
    },
    {
      id: "design",
      templateKey: "design",
      title: "Design",
      state: "proposed" as const,
      naReason: null,
      allowNa: true,
      openQuestionCount: 1,
      settledBy: null,
      stateChangedAt: "2026-08-13T09:00:00.000Z",
    },
  ],
};

const stateChip = {
  kind: "spec_section_state_changed" as const,
  specId: "spec-1",
  sectionId: "design",
  sectionTitle: "Design",
  before: { state: "proposed" as const, naReason: null },
  after: { state: "settled" as const, naReason: null },
  undo: {
    kind: "restore_section_state" as const,
    specId: "spec-1",
    sectionId: "design",
    expected: { state: "settled" as const, naReason: null },
    restore: { state: "proposed" as const, naReason: null },
  },
};

const setStateMutate = vi.fn(
  (
    _input: unknown,
    options?: { onSuccess?: (result: { chip: typeof stateChip; rail: typeof rail }) => void },
  ) => options?.onSuccess?.({ chip: stateChip, rail }),
);
const undoStateMutate = vi.fn(
  (
    _input: unknown,
    options?: { onSuccess?: (result: { chip: typeof stateChip; rail: typeof rail }) => void },
  ) => options?.onSuccess?.({ chip: stateChip, rail }),
);

vi.mock("@/components/spec", () => ({
  SpecChatRail: ({ sessionId }: { sessionId: string }) => (
    <aside aria-label="Drafting session">chat-rail:{sessionId}</aside>
  ),
  LazySpecCanvas: ({
    specId,
    revision,
    selectionActions,
  }: {
    specId: string;
    revision: string;
    selectionActions?: { onAction: (payload: SpecSelectionActionPayload) => void };
  }) => (
    <div aria-label="Collaborative spec canvas">
      {specId}:{revision}
      {selectionActions &&
        (["refine", "wrong", "cut", "ask", "custom"] as const).map((action) => (
          <button key={action} onClick={() => selectionActions.onAction(actionPayload(action))}>
            action-{action}
          </button>
        ))}
    </div>
  ),
}));

// After publish the canvas is the ticket tree, and the spec becomes the second
// tab (mock 2l). The tree itself is covered by SpecTicketTree.test.tsx.
vi.mock("@/hooks/useSpecTickets", () => ({
  useSpecTickets: () => ({
    data: {
      specId: "spec-1",
      checkpointId: pinned.id,
      docSeq: "18",
      publishedAt: pinned.createdAt,
      sections: [{ id: "context", title: "Context" }],
      tickets: [],
      unattachedQuestions: [],
    },
    isPending: false,
    error: null,
  }),
  useSpecTicketCommand: () => ({ mutateAsync: vi.fn() }),
  writeTree: vi.fn(),
}));

vi.mock("@/hooks/useSpecRead", () => ({
  useSpecRead: () => ({
    data: {
      spec: {
        id: "spec-1",
        title: "Safe restore design",
        lifecycle: view.lifecycle,
        sessionId: view.sessionId,
        publishedCheckpointId: view.publishedCheckpointId,
        publishedAt: view.lifecycle === "published" ? pinned.createdAt : null,
        revision: "17",
        template: {
          id: "00000000-0000-4000-8000-000000000115",
          name: "Engineering design doc",
        },
      },
      checkpoints: [
        {
          id: older.id,
          label: older.label,
          author: { id: "member-1", name: "Ada" },
          reason: older.reason,
          docSeq: older.docSeq,
          createdAt: older.createdAt,
        },
        {
          id: pinned.id,
          label: pinned.label,
          author: { id: "member-1", name: "Ada" },
          reason: pinned.reason,
          docSeq: pinned.docSeq,
          createdAt: pinned.createdAt,
        },
      ],
      publishedCheckpoint: view.lifecycle === "published" ? pinned : null,
    },
    isPending: false,
    error: null,
    refetch: readRefetch,
  }),
  useSpecRail: () => ({ data: rail, isPending: false, error: null }),
  useSpecCheckpoint: (_specId: string, checkpointId: string | null) => ({
    data: checkpointId === older.id ? older : checkpointId === pinned.id ? pinned : undefined,
    isPending: false,
  }),
  useRestoreSpecSection: () => ({ mutate: restoreMutate, isPending: false }),
  useSetSpecSectionState: () => ({ mutate: setStateMutate, isPending: false }),
  useUndoSpecSectionState: () => ({ mutate: undoStateMutate, isPending: false }),
}));

const desktopWidth = window.innerWidth;

function setWidth(pixels: number) {
  Object.defineProperty(window, "innerWidth", {
    configurable: true,
    writable: true,
    value: pixels,
  });
}

beforeEach(() => {
  setWidth(desktopWidth);
  view.lifecycle = "draft";
  view.sessionId = null;
  view.publishedCheckpointId = null;
  restoreMutate.mockReset();
  readRefetch.mockReset();
  setStateMutate.mockClear();
  undoStateMutate.mockClear();
});

describe("SpecReadPage", () => {
  it("joins a draft through the existing lazy canvas without collaborator chat", async () => {
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    expect((await screen.findByLabelText("Collaborative spec canvas")).textContent).toContain(
      "spec-1:17",
    );
    expect(screen.getByText("Live draft")).toBeTruthy();
    expect(screen.queryByText("Open owner session")).toBeNull();
    expect(screen.queryByLabelText("Drafting session")).toBeNull();
  });

  it("mounts the chat rail for the owner of a draft", async () => {
    view.sessionId = "session-1";
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    expect((await screen.findByLabelText("Drafting session")).textContent).toContain(
      "chat-rail:session-1",
    );
    expect(screen.getByLabelText("Collaborative spec canvas")).toBeTruthy();
  });

  it("keeps the chat rail off a published spec", async () => {
    view.sessionId = "session-1";
    view.lifecycle = "published";
    view.publishedCheckpointId = older.id;
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    expect(await screen.findByText("Published spec")).toBeTruthy();
    expect(screen.queryByLabelText("Drafting session")).toBeNull();
  });

  it("keeps the chat rail off a folded small screen", async () => {
    view.sessionId = "session-1";
    setWidth(420);
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    expect(await screen.findByLabelText("Collaborative spec canvas")).toBeTruthy();
    expect(screen.queryByLabelText("Drafting session")).toBeNull();
  });

  it("shows the locked template with the reason it cannot change", async () => {
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    const control = await screen.findByLabelText("Template");
    expect((control as HTMLSelectElement).disabled).toBe(true);
    expect(control.textContent).toBe("Engineering design doc");
    expect(screen.getByText("The template is locked once the session starts.")).toBeTruthy();
  });

  it("opens a published spec at the pinned read-only checkpoint", async () => {
    const user = userEvent.setup();
    view.lifecycle = "published";
    view.publishedCheckpointId = older.id;
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    // The tree leads; the pinned spec is one tab away and still read-only.
    expect(await screen.findByRole("list", { name: "Ticket tree" })).toBeTruthy();
    expect(screen.getByText("Published spec")).toBeTruthy();
    await user.click(screen.getByRole("tab", { name: "Spec v18 pinned" }));
    expect(await screen.findByText("Pinned content.")).toBeTruthy();
    await user.click(screen.getByRole("tab", { name: "Checkpoints" }));
    expect(screen.getByText("Pinned")).toBeTruthy();
    expect(screen.queryByLabelText("Collaborative spec canvas")).toBeNull();
  });

  it("returns to the server-pinned checkpoint when a draft becomes published", async () => {
    const user = userEvent.setup();
    renderWithProviders(<SpecReadPage specId="spec-1" />);
    await user.click(await screen.findByRole("tab", { name: "Checkpoints" }));
    fireEvent.click(await screen.findByRole("button", { name: /Initial outline/ }));
    expect(await screen.findByText("Initial content.")).toBeTruthy();

    view.lifecycle = "published";
    view.publishedCheckpointId = older.id;
    fireEvent.click(screen.getByRole("button", { name: /Ready to publish/ }));

    await user.click(await screen.findByRole("tab", { name: "Spec v18 pinned" }));
    expect(await screen.findByText("Pinned content.")).toBeTruthy();
    expect(screen.queryByText("Initial content.")).toBeNull();
  });

  it("shows the owner session link for the spec owner", async () => {
    view.sessionId = "session-1";
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    const link = await screen.findByRole("link", { name: "Open owner session" });
    expect(link.getAttribute("href")).toBe("/sessions/session-1");
  });

  it.each(["refine", "wrong", "cut", "ask", "custom"] as const)(
    "sends the complete %s selection through the owner session",
    async (action) => {
      view.sessionId = "session-1";
      const sent: Array<{ sessionId: string; text: string; promptId: string }> = [];
      const transport = createRouterTransport((router) => {
        router.service(SessionService, {
          sendPrompt: (request) => {
            sent.push({
              sessionId: request.sessionId,
              text: request.text,
              promptId: request.promptId,
            });
            return { sessionId: request.sessionId, note: "accepted" };
          },
        });
      });
      renderWithProviders(<SpecReadPage specId="spec-1" />, { transport });

      fireEvent.click(await screen.findByRole("button", { name: `action-${action}` }));
      await waitFor(() => expect(sent).toHaveLength(1));

      expect(sent[0]!.sessionId).toBe("session-1");
      expect(sent[0]!.promptId).not.toBe("");
      for (const expected of [
        `"action": "${action}"`,
        '"spec_id": "spec-1"',
        '"section_id": "failure-modes"',
        '"selection_spec_id": "spec-1"',
        '"selection_revision": "17"',
        '"selection_start": "start-anchor"',
        '"selection_end": "end-anchor"',
        '"selection_text": "Retry forever."',
        `"selection_fingerprint": "${"a".repeat(64)}"`,
      ]) {
        expect(sent[0]!.text).toContain(expected);
      }
      if (action === "ask") {
        expect(sent[0]!.text).toContain("Answer in chat");
        expect(sent[0]!.text).not.toContain("Call spec_update_section once");
      } else {
        expect(sent[0]!.text).toContain("Call spec_update_section once");
      }
    },
  );

  it("does not give a collaborator an agent-driving action handler", async () => {
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    expect(await screen.findByLabelText("Collaborative spec canvas")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "action-refine" })).toBeNull();
  });

  it("compares two checkpoints and restores one selected checkpoint section", async () => {
    const user = userEvent.setup();
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    await user.click(await screen.findByRole("tab", { name: "Checkpoints" }));
    await user.click(await screen.findByRole("button", { name: /Initial outline/ }));
    await user.click(screen.getByRole("button", { name: /Ready to publish/ }));
    expect(await screen.findByText("Checkpoint comparison")).toBeTruthy();
    expect(screen.getByText("Initial outline → Ready to publish")).toBeTruthy();

    await user.click(screen.getByRole("button", { name: /Initial outline/ }));
    await user.click(await screen.findByRole("button", { name: "Restore section" }));
    expect(restoreMutate).toHaveBeenCalledWith(
      { checkpointId: pinned.id, sectionId: "context" },
      expect.any(Object),
    );
  });

  it("refreshes server truth when restore races with publish", async () => {
    const user = userEvent.setup();
    renderWithProviders(<SpecReadPage specId="spec-1" />);
    await user.click(await screen.findByRole("tab", { name: "Checkpoints" }));

    fireEvent.click(await screen.findByRole("button", { name: /Ready to publish/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Restore section" }));
    const callbacks = restoreMutate.mock.calls[0]?.[1] as
      | { onError?: (error: unknown) => void }
      | undefined;
    callbacks?.onError?.({ status: 409 });

    expect(
      await screen.findByText(
        "This spec was published before the restore finished. The published version is read-only.",
      ),
    ).toBeTruthy();
    expect(readRefetch).toHaveBeenCalledTimes(1);
  });

  it("shows an undoable transcript chip after settle-from-rail", async () => {
    const user = userEvent.setup();
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    await user.click(await screen.findByRole("button", { name: "Settle" }));
    expect(await screen.findByRole("status", { name: "Design state changed" })).toBeTruthy();
    expect(setStateMutate).toHaveBeenCalledWith(
      {
        sectionId: "design",
        state: "settled",
        actionId: expect.any(String),
      },
      expect.any(Object),
    );

    await user.click(screen.getByRole("button", { name: "Undo" }));
    expect(undoStateMutate).toHaveBeenCalledWith(
      {
        sectionId: "design",
        undo: stateChip.undo,
        actionId: expect.any(String),
      },
      expect.any(Object),
    );
  });

  it("folds the rail into a tally and a flag count at 420px", async () => {
    setWidth(420);
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    const fold = await screen.findByRole("button", {
      name: "Spec sections. 1 of 2 complete. 1 open question.",
    });
    expect(fold.textContent).toContain("1/2 sections");
    // The rail gave up its column: no tab strip, and no visible section list.
    expect(screen.queryByRole("tab", { name: "Sections" })).toBeNull();
    expect(screen.queryByText("Completeness")).toBeNull();
    expect(await screen.findByLabelText("Collaborative spec canvas")).toBeTruthy();
  });

  it("settles a section from the folded rail's sheet", async () => {
    const user = userEvent.setup();
    setWidth(420);
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    await user.click(await screen.findByRole("button", { name: /^Spec sections\./ }));
    expect(await screen.findByRole("tab", { name: "Sections" })).toBeTruthy();

    await user.click(await screen.findByRole("button", { name: "Settle" }));

    expect(setStateMutate).toHaveBeenCalledWith(
      { sectionId: "design", state: "settled", actionId: expect.any(String) },
      expect.any(Object),
    );
    // The sheet is modal, so the chip behind it is out of the accessibility
    // tree until the reader closes the sheet.
    await user.keyboard("{Escape}");
    expect(await screen.findByRole("status", { name: "Design state changed" })).toBeTruthy();
  });

  it("attributes a checkpoint and marks its selection order", async () => {
    const user = userEvent.setup();
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    await user.click(await screen.findByRole("tab", { name: "Checkpoints" }));
    const checkpoint = screen.getByRole("button", { name: /Ready to publish/ });
    expect(within(checkpoint).getByText(/Ada/)).toBeTruthy();

    await user.click(checkpoint);
    const selected = screen.getByRole("button", { name: /Ready to publish/ });
    expect(selected.getAttribute("aria-pressed")).toBe("true");
    expect(within(selected).getByText("1")).toBeTruthy();
  });
});
