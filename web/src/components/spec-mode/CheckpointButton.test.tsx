import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import type { SpecCheckpointSummary } from "@/hooks/useSpecRead";
import { CheckpointButton } from "./CheckpointButton";

vi.mock("@/hooks/useSpecRead", () => ({
  useSpecCheckpoint: () => ({ data: undefined, isPending: false }),
}));

const checkpoint = (id: string, docSeq: number): SpecCheckpointSummary => ({
  id,
  label: `Checkpoint ${docSeq}`,
  author: null,
  reason: "test",
  docSeq: String(docSeq),
  createdAt: new Date(Date.UTC(2026, 0, docSeq)).toISOString(),
});

describe("CheckpointButton", () => {
  it("keeps a manual comparison when a new checkpoint arrives while the sheet is open", async () => {
    const user = userEvent.setup();
    const initialCheckpoints = [
      checkpoint("checkpoint-1", 1),
      checkpoint("checkpoint-2", 2),
      checkpoint("checkpoint-3", 3),
    ];
    const view = render(<CheckpointButton specId="spec-1" checkpoints={initialCheckpoints} />);

    await user.click(screen.getByRole("button", { name: /History/ }));
    await user.click(screen.getByRole("button", { name: /Checkpoint 1/ }));
    expect(screen.getByRole("button", { name: /Checkpoint 1/ }).getAttribute("aria-pressed")).toBe(
      "true",
    );
    expect(screen.getByRole("button", { name: /Checkpoint 2/ }).getAttribute("aria-pressed")).toBe(
      "false",
    );

    view.rerender(
      <CheckpointButton
        specId="spec-1"
        checkpoints={[...initialCheckpoints, checkpoint("checkpoint-4", 4)]}
      />,
    );

    expect(screen.getByRole("button", { name: /Checkpoint 1/ }).getAttribute("aria-pressed")).toBe(
      "true",
    );
    expect(screen.getByRole("button", { name: /Checkpoint 3/ }).getAttribute("aria-pressed")).toBe(
      "true",
    );
    expect(screen.getByRole("button", { name: /Checkpoint 4/ }).getAttribute("aria-pressed")).toBe(
      "false",
    );
  });
});
