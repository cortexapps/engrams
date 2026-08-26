import { describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";

import { AutomationRunService, type ListRunsRequest } from "@/gen/engram/app/v1/automation_pb";
import { renderWithProviders } from "@/test-utils";
import { RunsTab, interleaveRuns } from "./RunsTab";

const NOW = Date.parse("2026-08-21T12:00:00Z");

function brief(
  id: string,
  status: string,
  minutesAgo: number,
  extra: Record<string, unknown> = {},
) {
  const createdAt = new Date(NOW - minutesAgo * 60_000).toISOString();
  return {
    id,
    automationId: "auto-1",
    version: 1,
    status,
    triggerSource: "integration",
    eventKey: "pull_request.opened",
    dryRun: false,
    startedAt: createdAt,
    endedAt:
      status === "running" ? undefined : new Date(NOW - minutesAgo * 60_000 + 30_000).toISOString(),
    createdAt,
    ...extra,
  };
}

type ListImpl = (req: ListRunsRequest) => {
  runs: ReturnType<typeof brief>[];
  filtered: Array<{
    count: number;
    firstAt: string;
    lastAt: string;
    beforeRunId: string;
  }>;
};

function transportWith(listRuns: ListImpl) {
  return createRouterTransport((router) => {
    router.service(AutomationRunService, {
      listRuns,
      getRun: () => ({ run: undefined }),
      stopRun: () => ({ sent: true }),
      retryRun: () => ({ runId: "" }),
    });
  });
}

describe("interleaveRuns", () => {
  it("places each filtered window before the run it precedes and trailing windows last", () => {
    const rows = interleaveRuns(
      [brief("r1", "completed", 5), brief("r2", "failed", 60)] as never,
      [
        { count: 3, firstAt: "a", lastAt: "b", beforeRunId: "r2" },
        { count: 1, firstAt: "c", lastAt: "d", beforeRunId: "" },
      ] as never,
    );
    expect(rows.map((r) => (r.kind === "run" ? r.run.id : `w${r.window.count}`))).toEqual([
      "r1",
      "w3",
      "r2",
      "w1",
    ]);
  });
});

describe("RunsTab", () => {
  it("renders status dots per run and collapses filtered windows until expanded", async () => {
    const listRuns = vi.fn<ListImpl>(() => ({
      runs: [
        brief("autorun:auto-1:github:d1", "completed", 5),
        brief("autorun:auto-1:github:d2", "failed", 60),
      ],
      filtered: [
        {
          count: 4,
          firstAt: new Date(NOW - 50 * 60_000).toISOString(),
          lastAt: new Date(NOW - 10 * 60_000).toISOString(),
          beforeRunId: "autorun:auto-1:github:d2",
        },
      ],
    }));
    renderWithProviders(<RunsTab automationId="auto-1" now={() => NOW} />, {
      transport: transportWith(listRuns),
    });

    const rows = await screen.findAllByTestId("run-row");
    expect(rows).toHaveLength(2);
    expect(screen.getByRole("img", { name: "status completed" }).getAttribute("data-tone")).toBe(
      "nominal",
    );
    expect(screen.getByRole("img", { name: "status failed" }).getAttribute("data-tone")).toBe(
      "critical",
    );

    const window = screen.getByRole("button", { name: /4 filtered events/ });
    expect(window.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText(/did not pass the automation's filter/)).toBeNull();
    await userEvent.setup().click(window);
    expect(window.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText(/did not pass the automation's filter/)).toBeTruthy();
  });

  it("refetches with include_filtered when the toggle flips", async () => {
    const listRuns = vi.fn<ListImpl>(() => ({ runs: [], filtered: [] }));
    renderWithProviders(<RunsTab automationId="auto-1" now={() => NOW} />, {
      transport: transportWith(listRuns),
    });
    await screen.findByText(/no runs yet/i);
    expect(listRuns).toHaveBeenLastCalledWith(
      expect.objectContaining({
        automationId: "auto-1",
        includeFiltered: false,
      }),
      expect.anything(),
    );

    await userEvent.setup().click(screen.getByRole("switch", { name: /show filtered runs/i }));
    await waitFor(() =>
      expect(listRuns).toHaveBeenLastCalledWith(
        expect.objectContaining({ includeFiltered: true }),
        expect.anything(),
      ),
    );
  });
});

describe("workstream chips (ADR 0120)", () => {
  it("labels an instance-bound run by its workstream key; unbound rows get no chip", async () => {
    const transport = createRouterTransport((router) => {
      router.service(AutomationRunService, {
        listRuns: () => ({
          runs: [
            brief("run-1", "completed", 5, { instanceId: "ai_one" }),
            brief("run-2", "completed", 9),
          ],
          filtered: [],
        }),
        listInstances: () => ({
          instances: [
            {
              id: "ai_one",
              automationId: "auto-1",
              key: "project-ENG-1",
              status: "open",
              inputsJson: "{}",
              openedBy: "",
              openedAt: new Date(NOW - 60_000).toISOString(),
            },
          ],
        }),
        getRun: () => ({ run: undefined }),
      });
    });
    renderWithProviders(<RunsTab automationId="auto-1" now={() => NOW} />, {
      transport,
    });
    await waitFor(() => expect(screen.getAllByTestId("run-row")).toHaveLength(2));
    await waitFor(() =>
      expect(screen.getByTestId("run-workstream-chip").textContent).toBe("project-ENG-1"),
    );
    expect(screen.getAllByTestId("run-workstream-chip")).toHaveLength(1);
  });
});
