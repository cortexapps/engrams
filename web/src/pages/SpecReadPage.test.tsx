import { screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { renderWithProviders } from "@/test-utils";
import { SpecReadPage } from "./SpecReadPage";

const view: { lifecycle: "draft" | "published"; sessionId: string | null } = {
  lifecycle: "draft",
  sessionId: null,
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
        publishedCheckpointId: view.lifecycle === "published" ? pinned.id : null,
        publishedAt: view.lifecycle === "published" ? pinned.createdAt : null,
      },
      checkpoints: [
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
  }),
  useSpecCheckpoint: (_specId: string, checkpointId: string | null) => ({
    data: checkpointId ? pinned : undefined,
    isPending: false,
  }),
  useRestoreSpecSection: () => ({ mutate: vi.fn(), isPending: false }),
}));

beforeEach(() => {
  view.lifecycle = "draft";
  view.sessionId = null;
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
    renderWithProviders(<SpecReadPage specId="spec-1" />);

    expect(await screen.findByText("Pinned content.")).toBeTruthy();
    expect(screen.getByText("Published spec")).toBeTruthy();
    expect(screen.getByText("Pinned")).toBeTruthy();
    expect(screen.queryByLabelText("Collaborative spec canvas")).toBeNull();
  });
});
