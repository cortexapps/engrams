import { describe, expect, it, vi } from "vitest";
import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { renderWithProviders } from "@/test-utils";
import { RunTimeline } from "./RunTimeline";
import { buildTimeline, parseFramePath } from "./run-format";

const T0 = "2026-08-21T12:00:00Z";

function step(blockId: string, status = "succeeded", attempt = 0) {
  return {
    blockId,
    attempt,
    status,
    inputsJson: "{}",
    outputsJson: "{}",
    startedAt: T0,
    endedAt: T0,
  };
}

describe("parseFramePath / buildTimeline", () => {
  it("parses iteration-suffixed frames", () => {
    expect(parseFramePath("poll[2].tick")).toEqual([
      { blockId: "poll", iteration: 2 },
      { blockId: "tick" },
    ]);
    expect(parseFramePath("gate.hot_path")).toEqual([{ blockId: "gate" }, { blockId: "hot_path" }]);
  });

  it("groups loop iterations, nests branch arms, collapses attempts, hides auxiliary rows", () => {
    const nodes = buildTimeline([
      step("launch"),
      step("poll[0].tick", "failed", 0),
      step("poll[0].tick", "succeeded", 1),
      step("poll[0].__until__"),
      step("poll[1].tick"),
      step("gate.__cond__"),
      step("gate.hot_path"),
    ] as never);

    expect(
      nodes.map((n) => (n.kind === "step" ? n.step.blockId : `iter:${n.loopId}[${n.iteration}]`)),
    ).toEqual(["launch", "iter:poll[0]", "iter:poll[1]", "hot_path"]);
    const first = nodes[1];
    if (first?.kind !== "iteration") throw new Error("expected iteration group");
    const tick = first.steps[0];
    if (tick?.kind !== "step") throw new Error("expected step");
    expect(tick.step.attempt).toBe(1);
    expect(tick.step.attempts).toHaveLength(2);
    expect(tick.step.latest.status).toBe("succeeded");
    const arm = nodes[3];
    if (arm?.kind !== "step") throw new Error("expected step");
    expect(arm.step.depth).toBe(1);
  });
});

describe("RunTimeline", () => {
  it("renders iteration groups, indented branch arms, and attempt badges; selects on click", async () => {
    const onSelect = vi.fn();
    renderWithProviders(
      <RunTimeline
        steps={
          [
            step("launch"),
            step("poll[0].tick", "failed", 0),
            step("poll[0].tick", "succeeded", 1),
            step("gate.hot_path"),
          ] as never
        }
        blockTypes={{ launch: "create_session", tick: "run_command", hot_path: "send_prompt" }}
        onSelect={onSelect}
      />,
    );

    const groups = await screen.findAllByTestId("iteration-group");
    expect(groups).toHaveLength(1);
    expect(within(groups[0]!).getByRole("button", { name: /poll · iteration 1/i })).toBeTruthy();
    expect(within(groups[0]!).getByTestId("attempt").textContent).toBe("attempt 2");

    const rows = screen.getAllByTestId("timeline-step");
    const arm = rows.find((r) => r.textContent?.includes("hot_path"))!;
    expect(arm.closest("li")?.getAttribute("data-depth")).toBe("1");
    expect(rows[0]!.closest("li")?.getAttribute("data-depth")).toBe("0");

    await userEvent.setup().click(arm);
    expect(onSelect).toHaveBeenCalledWith(
      expect.objectContaining({ blockId: "hot_path", path: "gate.hot_path" }),
    );
  });
});
