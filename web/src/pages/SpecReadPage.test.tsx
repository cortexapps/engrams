import { fireEvent, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { renderWithProviders } from "@/test-utils";
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

vi.mock("@/components/spec", () => ({
  LazySpecCanvas: ({ specId }: { specId: string }) => (
    <div aria-label="Collaborative spec canvas">{specId}</div>
  ),
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
  useSpecCheckpoint: (_specId: string, checkpointId: string | null) => ({
    data: checkpointId === older.id ? older : checkpointId === pinned.id ? pinned : undefined,
    isPending: false,
  }),
  useRestoreSpecSection: () => ({ mutate: restoreMutate, isPending: false }),
}));

beforeEach(() => {
  view.lifecycle = "draft";
  view.sessionId = null;
  view.publishedCheckpointId = null;
  restoreMutate.mockReset();
  readRefetch.mockReset();
});

describe("SpecReadPage", () => {
  it("joins a draft through the existing lazy canvas without collaborator chat", async () => {
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    expect((await screen.findByLabelText("Collaborative spec canvas")).textContent).toBe("spec-1");
    expect(screen.getByText("Live draft")).toBeTruthy();
    expect(screen.queryByText("Open owner session")).toBeNull();
  });

  it("opens a published spec at the pinned read-only checkpoint", async () => {
    view.lifecycle = "published";
    view.publishedCheckpointId = older.id;
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    expect(await screen.findByText("Pinned content.")).toBeTruthy();
    expect(screen.getByText("Published spec")).toBeTruthy();
    expect(screen.getByText("Pinned")).toBeTruthy();
    expect(screen.queryByLabelText("Collaborative spec canvas")).toBeNull();
  });

  it("returns to the server-pinned checkpoint when a draft becomes published", async () => {
    renderWithProviders(<SpecReadPage specId="spec-1" />);
    fireEvent.click(await screen.findByRole("button", { name: /Initial outline/ }));
    expect(await screen.findByText("Initial content.")).toBeTruthy();

    view.lifecycle = "published";
    view.publishedCheckpointId = older.id;
    fireEvent.click(screen.getByRole("button", { name: /Ready to publish/ }));

    expect(await screen.findByText("Pinned content.")).toBeTruthy();
    expect(screen.queryByText("Initial content.")).toBeNull();
  });

  it("shows the owner session link for the spec owner", async () => {
    view.sessionId = "session-1";
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    const link = await screen.findByRole("link", { name: "Open owner session" });
    expect(link.getAttribute("href")).toBe("/sessions/session-1");
  });

  it("compares two checkpoints and restores one selected checkpoint section", async () => {
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    fireEvent.click(await screen.findByRole("button", { name: /Initial outline/ }));
    fireEvent.click(screen.getByRole("button", { name: /Ready to publish/ }));
    expect(await screen.findByText("Checkpoint comparison")).toBeTruthy();
    expect(screen.getByText("Initial outline → Ready to publish")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: /Initial outline/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Restore section" }));
    expect(restoreMutate).toHaveBeenCalledWith(
      { checkpointId: pinned.id, sectionId: "context" },
      expect.any(Object),
    );
  });

  it("refreshes server truth when restore races with publish", async () => {
    renderWithProviders(<SpecReadPage specId="spec-1" />);

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
});
