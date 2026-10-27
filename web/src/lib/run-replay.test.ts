import { describe, expect, it } from "vitest";

import { replayMap, stepStatus, wayOnPath } from "./run-replay";

const NOW = Date.parse("2026-08-21T12:00:00Z");

function row(blockId: string, status: string, extra: Record<string, unknown> = {}) {
  return {
    blockId,
    attempt: 0,
    status,
    inputsJson: "{}",
    outputsJson: "{}",
    startedAt: new Date(NOW - 10_000).toISOString(),
    endedAt: new Date(NOW - 9_000).toISOString(),
    ...extra,
  } as never;
}

const TYPES: Record<string, string> = {
  admit: "filter",
  agent: "run_agent",
  poll: "loop",
  check: "session_status",
};

describe("replayMap", () => {
  it("folds the ledger by block with a sentence, a status and a duration", () => {
    const map = replayMap(
      [
        row("admit", "succeeded", { outputsJson: JSON.stringify({ pass: true }) }),
        row("agent", "failed", { error: "no host had capacity" }),
      ],
      (id) => TYPES[id],
      NOW,
    );
    expect(map.get("admit")).toMatchObject({ status: "ok", sentence: "Passed", duration: "1s" });
    expect(map.get("agent")).toMatchObject({ status: "failed", sentence: "no host had capacity" });
    expect(map.has("linked")).toBe(false);
  });

  it("a looped block keeps every iteration and shows the last", () => {
    const map = replayMap(
      [
        row("poll", "succeeded"),
        row("poll[0].check", "succeeded", { outputsJson: JSON.stringify({ status: "running" }) }),
        row("poll[1].check", "succeeded", { outputsJson: JSON.stringify({ status: "idle" }) }),
      ],
      (id) => TYPES[id],
      NOW,
    );
    const check = map.get("check")!;
    expect(check.frames.map((f) => f.path)).toEqual(["poll[0].check", "poll[1].check"]);
    expect(check.sentence).toBe("The session is idle");
  });

  it("names the way in that carried the run", () => {
    const map = replayMap([row("admit", "skipped")], (id) => TYPES[id], NOW);
    expect(wayOnPath(map, ["admit", "agent"])).toBe(true);
    expect(wayOnPath(map, ["close"])).toBe(false);
    expect(stepStatus("deadline")).toBe("failed");
    expect(stepStatus("running")).toBe("running");
  });
});
