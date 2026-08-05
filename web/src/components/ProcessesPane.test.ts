import { describe, expect, it } from "vitest";
import { deriveBashCommands } from "./ProcessesPane";
import type { IndexedEvent } from "../lib/types";

function started(id: string, command: string, truncated = false): IndexedEvent {
  const summary = JSON.stringify({ command });
  return {
    idx: 0,
    event: {
      type: "tool_call_started",
      run_id: "r1",
      tool_call_id: id,
      tool_name: "Bash",
      args_summary: truncated ? summary.slice(0, summary.length - 2) : summary,
      at: "2026-08-04T00:00:00Z",
    },
  };
}

function completed(id: string, ok: boolean): IndexedEvent {
  return {
    idx: 0,
    event: {
      type: "tool_call_completed",
      run_id: "r1",
      tool_call_id: id,
      tool_name: "Bash",
      ok,
      duration_ms: 1500,
      result_summary: null,
      at: "2026-08-04T00:00:01Z",
    },
  };
}

describe("deriveBashCommands", () => {
  it("lists running commands first, newest first, and marks completions", () => {
    const rows = deriveBashCommands([
      started("a", "sleep 100"),
      completed("a", true),
      started("b", "make build"),
      started("c", "npm test"),
    ]);
    expect(rows.map((r) => r.toolCallId)).toEqual(["c", "b", "a"]);
    expect(rows[0].done).toBe(false);
    expect(rows[2]).toMatchObject({ done: true, ok: true, durationMs: 1500 });
    expect(rows[1].command).toBe("make build");
  });

  it("ignores non-Bash tools and falls back to raw text on truncated args", () => {
    const rows = deriveBashCommands([
      started("t", "echo hi", true),
      {
        idx: 0,
        event: {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "read1",
          tool_name: "Read",
          args_summary: "{}",
          at: "2026-08-04T00:00:00Z",
        },
      },
    ]);
    expect(rows).toHaveLength(1);
    // Truncated JSON does not parse — the raw summary is shown as-is.
    expect(rows[0].command).toContain("echo hi");
  });
});
