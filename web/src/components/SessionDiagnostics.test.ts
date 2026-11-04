import { describe, it, expect } from "vitest";
import { durabilitySummary } from "./SessionDiagnostics";
import type { CheckpointSummary } from "../lib/types";

// The durability telltale's honest-state contract: it reassures when there's
// something true + calming to say, and stays silent (returns null) rather than
// alarm or emit operator noise. These cases pin that behaviour per lifecycle
// state so a refactor can't quietly turn silence into a false "work saved".

function ckpt(partial: Partial<CheckpointSummary> = {}): CheckpointSummary {
  return {
    snapshot_id: "c-1",
    created_at: new Date().toISOString(),
    size_bytes: 1024,
    events_cursor: 1,
    recoverable: true,
    is_latest: true,
    ...partial,
  };
}

describe("durabilitySummary", () => {
  it("stays silent on terminal states (the status glyph already carries them)", () => {
    expect(durabilitySummary("completed", [ckpt()])).toBeNull();
    expect(durabilitySummary("failed", [ckpt()])).toBeNull();
    expect(durabilitySummary("dead", [ckpt()])).toBeNull();
  });

  it("reassures suspended/transitional states only when a recoverable point exists", () => {
    const safe = durabilitySummary("idle", [ckpt({ recoverable: true })]);
    expect(safe).toEqual({
      tone: "nominal",
      label: "Safe · recoverable",
      title: expect.any(String),
    });

    // No recoverable checkpoint → silent, never an alarm.
    expect(durabilitySummary("idle", [ckpt({ recoverable: false })])).toBeNull();
    expect(durabilitySummary("host_lost", [])).toBeNull();
  });

  it("confirms saved work on an active session from its newest checkpoint", () => {
    const checkpointed = durabilitySummary("active", [ckpt(), ckpt({ snapshot_id: "c-0" })]);
    expect(checkpointed?.tone).toBe("nominal");
    expect(checkpointed?.label.startsWith("Work saved ·")).toBe(true);
    expect(checkpointed?.title.endsWith("2 checkpoints")).toBe(true);
  });

  it("stays silent before the first checkpoint", () => {
    expect(durabilitySummary("active", [])).toBeNull();
    expect(durabilitySummary("pending", [])).toBeNull();
    expect(durabilitySummary("created", [])).toBeNull();
  });
});
