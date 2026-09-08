import { describe, expect, it, vi } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";

import { AutomationRunService, type ListRunsRequest } from "@/gen/engram/app/v1/automation_pb";
import { renderWithProviders } from "@/test-utils";
import { ActivityTab } from "./ActivityTab";
import {
  countToday,
  entryTitle,
  entryVia,
  interleaveRuns,
  matchesFilter,
  windowSummary,
} from "./activity-format";

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
    instanceId: "",
    ...extra,
  };
}

type Brief = ReturnType<typeof brief>;
type ListImpl = (req: ListRunsRequest) => {
  runs: Brief[];
  filtered: Array<{ count: number; firstAt: string; lastAt: string; beforeRunId: string }>;
};

function transportWith(listRuns: ListImpl, steps: Record<string, unknown>[] = []) {
  return createRouterTransport((router) => {
    router.service(AutomationRunService, {
      listRuns,
      getRun: (req) => ({
        run: {
          brief: brief(req.runId, "failed", 60, { error: "verifier exited 1" }),
          triggerJson: "{}",
          steps,
          sessionIds: ["se-finder-1"],
        },
      }),
      stopRun: () => ({ sent: true }),
      retryRun: () => ({ runId: "" }),
      listInstances: () => ({ instances: [] }),
    });
  });
}

describe("activity-format", () => {
  it("titles an entry by what got in, in words", () => {
    expect(entryTitle(brief("r", "completed", 1) as never)).toBe("pull request · opened");
    expect(entryTitle(brief("r", "completed", 1, { triggerSource: "cron" }) as never)).toBe(
      "Scheduled run",
    );
    expect(
      entryTitle(brief("r", "completed", 1, { triggerSource: "manual", dryRun: true }) as never),
    ).toBe("Dry run");
    expect(entryTitle(brief("r", "filtered", 1) as never)).toBe("Filtered · pull request · opened");
    expect(entryVia(brief("r", "completed", 1) as never)).toBe(
      "via integration · pull_request.opened",
    );
  });

  it("folds a window into one sentence and names the automations on the cross ledger", () => {
    const text = windowSummary(
      [
        {
          count: 14,
          firstAt: "2026-08-21T09:02:00Z",
          lastAt: "2026-08-21T09:41:00Z",
          beforeRunId: "",
          automationId: "a1",
        } as never,
      ],
      (id) => (id === "a1" ? "Slack digest" : undefined),
    );
    expect(text).toMatch(/^14 deliveries filtered between .+ and .+ · Slack digest$/);
  });

  it("filters and counts", () => {
    expect(matchesFilter(brief("r", "failed", 1) as never, "failed")).toBe(true);
    expect(matchesFilter(brief("r", "running", 1) as never, "running")).toBe(true);
    expect(matchesFilter(brief("r", "completed", 1) as never, "superseded")).toBe(false);
    expect(
      countToday([brief("r", "completed", 1), brief("s", "completed", 60 * 48)] as never, NOW),
    ).toBe(1);
  });

  it("interleaves windows before the run they precede", () => {
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

describe("ActivityTab", () => {
  it("renders one entry per run, folds a filtered window, and filters by pill", async () => {
    const listRuns = vi.fn<ListImpl>(() => ({
      runs: [brief("run-ok", "completed", 5), brief("run-bad", "failed", 60)],
      filtered: [
        {
          count: 4,
          firstAt: new Date(NOW - 50 * 60_000).toISOString(),
          lastAt: new Date(NOW - 10 * 60_000).toISOString(),
          beforeRunId: "run-bad",
        },
      ],
    }));
    renderWithProviders(<ActivityTab automationId="auto-1" now={() => NOW} />, {
      transport: transportWith(listRuns),
    });

    const entries = await screen.findAllByTestId("activity-entry");
    expect(entries).toHaveLength(2);
    expect(entries[0]!.getAttribute("data-status")).toBe("completed");
    expect(within(entries[0]!).getByRole("img", { name: "status completed" })).toBeTruthy();
    // The filtered deliveries are one quiet row, not four entries.
    expect(screen.getByTestId("filtered-window").textContent).toMatch(/^4 deliveries filtered/);

    await userEvent.click(screen.getByRole("tab", { name: /failed/i }));
    expect(screen.getAllByTestId("activity-entry")).toHaveLength(1);
    expect(screen.queryByTestId("filtered-window")).toBeNull();
  });

  it("unfolds a step trace inline with the failed step tinted, its error, and the actions", async () => {
    const listRuns = vi.fn<ListImpl>(() => ({
      runs: [brief("run-bad", "failed", 60)],
      filtered: [],
    }));
    const steps = [
      {
        blockId: "clone",
        attempt: 0,
        status: "succeeded",
        inputsJson: "{}",
        outputsJson: JSON.stringify({ exit_status: 0 }),
        startedAt: new Date(NOW - 60 * 60_000).toISOString(),
        endedAt: new Date(NOW - 60 * 60_000 + 9_000).toISOString(),
      },
      {
        blockId: "verify",
        attempt: 1,
        status: "failed",
        inputsJson: "{}",
        outputsJson: "{}",
        error: "verifier exited 1",
        startedAt: new Date(NOW - 59 * 60_000).toISOString(),
        endedAt: new Date(NOW - 59 * 60_000 + 3_000).toISOString(),
      },
    ];
    renderWithProviders(<ActivityTab automationId="auto-1" now={() => NOW} />, {
      transport: transportWith(listRuns, steps),
    });

    await userEvent.click(await screen.findByRole("button", { expanded: false }));
    const trace = await screen.findByTestId("inline-trace");
    const rows = await within(trace).findAllByTestId("trace-step");
    expect(rows).toHaveLength(2);
    expect(rows[1]!.getAttribute("data-status")).toBe("failed");
    expect(within(rows[1]!).getByText("verifier exited 1")).toBeTruthy();
    expect(within(rows[0]!).getByText(/exit_status 0/)).toBeTruthy();
    expect(within(trace).getByRole("button", { name: "Retry" })).toBeTruthy();
    expect(
      within(trace)
        .getByRole("link", { name: /open session/i })
        .getAttribute("href"),
    ).toBe("/sessions/se-finder-1");
  });

  it("refetches with include_filtered when the switch flips", async () => {
    const listRuns = vi.fn<ListImpl>(() => ({ runs: [], filtered: [] }));
    renderWithProviders(<ActivityTab automationId="auto-1" now={() => NOW} />, {
      transport: transportWith(listRuns),
    });
    await screen.findByText(/no activity yet/i);
    expect(listRuns).toHaveBeenLastCalledWith(
      expect.objectContaining({ automationId: "auto-1", includeFiltered: false }),
      expect.anything(),
    );
    await userEvent.click(screen.getByRole("switch", { name: /show filtered deliveries/i }));
    await waitFor(() =>
      expect(listRuns).toHaveBeenLastCalledWith(
        expect.objectContaining({ includeFiltered: true }),
        expect.anything(),
      ),
    );
  });
});
