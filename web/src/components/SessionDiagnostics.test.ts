import { describe, it, expect } from "vitest";
import { durabilitySummary } from "./SessionDiagnostics";
import type { CheckpointSummary, CowStateView } from "../lib/types";

// The durability telltale's honest-state contract: it reassures when there's
// something true + calming to say, and stays silent (returns null) rather than
// alarm or emit operator noise. These cases pin that behaviour per lifecycle
// state so a refactor can't quietly turn "saving…" into a false "work saved".

function cow(partial: Partial<CowStateView> = {}): CowStateView {
  return {
    sandbox_id: "sb-1",
    session_id: "s-1",
    disk_manifest_id: "m-1",
    disk_manifest_version: 1,
    dirty_chunks: 0,
    dirty_bytes: 0,
    last_flush_at: null,
    base_chunks: 10,
    base_chunks_local: 10,
    memory_manifest_id: null,
    memory_manifest_version: null,
    last_snapshot_at: null,
    ...partial,
  };
}

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
    expect(durabilitySummary("completed", cow(), [])).toBeNull();
    expect(durabilitySummary("failed", cow(), [])).toBeNull();
    expect(durabilitySummary("dead", cow(), [])).toBeNull();
  });

  it("reassures suspended/transitional states only when a recoverable point exists", () => {
    const safe = durabilitySummary("idle", null, [ckpt({ recoverable: true })]);
    expect(safe).toEqual({
      tone: "nominal",
      label: "safe · recoverable",
      title: expect.any(String),
    });

    // No recoverable checkpoint → silent, never an alarm.
    expect(durabilitySummary("idle", null, [ckpt({ recoverable: false })])).toBeNull();
    expect(durabilitySummary("host_lost", null, [])).toBeNull();
  });

  it("confirms saved work on an active session with a durable anchor", () => {
    const flushed = durabilitySummary(
      "active",
      cow({ last_flush_at: new Date().toISOString() }),
      [],
    );
    expect(flushed?.tone).toBe("nominal");
    expect(flushed?.label.startsWith("work saved ·")).toBe(true);

    // A recent checkpoint counts as an anchor even with no flush yet.
    const checkpointed = durabilitySummary("active", cow(), [ckpt()]);
    expect(checkpointed?.label.startsWith("work saved ·")).toBe(true);
  });

  it("flags dirty-but-undurable work as in-flight, calmly (caution, not alarm)", () => {
    const saving = durabilitySummary("active", cow({ dirty_chunks: 3 }), []);
    expect(saving).toEqual({ tone: "caution", label: "saving…", title: expect.any(String) });
  });

  it("stays silent when there's no live telemetry or nothing written yet", () => {
    // Active but the host hasn't wired the COW pipeline → no operator noise here.
    expect(durabilitySummary("active", null, [])).toBeNull();
    // A clean slate with nothing dirty and nothing saved.
    expect(durabilitySummary("active", cow({ dirty_chunks: 0 }), [])).toBeNull();
    // Booting states with no telemetry.
    expect(durabilitySummary("pending", null, [])).toBeNull();
    expect(durabilitySummary("created", null, [])).toBeNull();
  });
});
