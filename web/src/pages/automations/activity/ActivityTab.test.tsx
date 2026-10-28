import { describe, expect, it, vi } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";

import {
  AutomationRunService,
  AutomationService,
  type ListRunsRequest,
} from "@/gen/engram/app/v1/automation_pb";
import { renderWithProviders } from "@/test-utils";
import { ActivityTab } from "./ActivityTab";
import {
  countToday,
  dropReasonText,
  entryTitle,
  entryVia,
  filteredReason,
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

const DEFINITION = {
  engine: 1,
  trigger: { kind: "integration", provider: "github", eventKeys: ["pull_request.opened"] },
  blocks: [
    { id: "clone", type: "run_command", config: {} },
    { id: "verify", type: "send_prompt", config: {} },
  ],
  inputsSchema: [],
  settings: { endSessionsOnFinish: false },
};

function transportWith(
  listRuns: ListImpl,
  steps: Record<string, unknown>[] = [],
  drops: Record<string, unknown>[] = [],
) {
  return createRouterTransport((router) => {
    router.service(AutomationService, {
      getAutomation: () => ({
        automation: {
          id: "auto-1",
          name: "Pull request review",
          version: { definitionJson: JSON.stringify(DEFINITION) },
        },
      }),
    });
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
      listRecentDrops: () => ({ drops }),
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
    expect(
      rows.map((r) =>
        r.kind === "run" ? r.run.id : r.kind === "window" ? `w${r.window.count}` : "drop",
      ),
    ).toEqual(["r1", "w3", "r2", "w1"]);
  });

  it("slots a drop in by time, before the first run older than it", () => {
    const drop = (minutesAgo: number) => ({
      entrypointId: "main",
      eventKey: "issue_comment.created",
      reason: "render_failed",
      detail: "undefined variable: event.raw.pull_request",
      droppedAt: new Date(NOW - minutesAgo * 60_000).toISOString(),
    });
    const rows = interleaveRuns(
      [brief("r1", "completed", 5), brief("r2", "failed", 60)] as never,
      [],
      [drop(30), drop(2), drop(90)] as never,
    );
    expect(
      rows.map((r) => (r.kind === "run" ? r.run.id : `d${r.kind === "drop" ? "" : "?"}`)),
    ).toEqual(["d", "r1", "d", "r2", "d"]);
  });

  it("says why a delivery did not fire, and a filtered run's filter", () => {
    expect(dropReasonText("render_failed")).toBe("had no key for the event");
    expect(dropReasonText("no_open_instance")).toBe("matched no open agent");
    expect(dropReasonText("closed_instance")).toBe("arrived for a closed agent");
    expect(
      filteredReason(
        brief("r", "filtered", 1, {
          error: 'admission: block "skip_bots" filtered the event',
        }) as never,
      ),
    ).toBe("skip_bots filtered the event");
    expect(filteredReason(brief("r", "completed", 1) as never)).toBeUndefined();
    // One delivery is "at", not "between X and X".
    const at = "2026-08-21T09:02:00Z";
    expect(
      windowSummary([{ count: 1, firstAt: at, lastAt: at, beforeRunId: "" }] as never),
    ).toMatch(/^1 delivery filtered at .+$/);
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

  it("shows a delivery that did not fire in its place, and a filtered run's filter", async () => {
    const listRuns = vi.fn<ListImpl>(() => ({
      runs: [
        brief("run-ok", "completed", 5),
        brief("run-skip", "filtered", 20, {
          startedAt: "",
          endedAt: undefined,
          error: 'admission: block "skip_bots" filtered the event',
        }),
      ],
      filtered: [],
    }));
    const drops = [
      {
        entrypointId: "comment",
        eventKey: "issue_comment.created",
        reason: "render_failed",
        detail: "undefined variable: event.raw.pull_request",
        droppedAt: new Date(NOW - 10 * 60_000).toISOString(),
      },
    ];
    renderWithProviders(<ActivityTab automationId="auto-1" now={() => NOW} />, {
      transport: transportWith(listRuns, [], drops),
    });

    const drop = await screen.findByTestId("activity-drop");
    expect(drop.textContent).toContain("Did not fire · issue comment · created");
    expect(drop.textContent).toContain("had no key for the event · undefined variable");
    // Newest first: the run 5 minutes ago, the drop 10 minutes ago, the run 20 minutes ago.
    const list = screen.getByTestId("activity-list");
    const kinds = [...list.querySelectorAll("[data-testid]")]
      .map((el) => el.getAttribute("data-testid"))
      .filter((id) => id === "activity-entry" || id === "activity-drop");
    expect(kinds).toEqual(["activity-entry", "activity-drop", "activity-entry"]);
    // The filtered run names its filter and shows no duration.
    const filtered = screen.getAllByTestId("activity-entry")[1]!;
    expect(within(filtered).getByTestId("entry-reason").textContent).toBe(
      "skip_bots filtered the event",
    );
    expect(filtered.textContent).not.toContain("—");
  });

  it("unfolds a step trace inline with the failed step tinted, its error, and the actions", async () => {
    const listRuns = vi.fn<ListImpl>(() => ({
      runs: [brief("run-bad", "failed", 60, { error: "verifier exited 1" })],
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
    // A step says what its block did, in the words of its kind.
    expect(within(rows[0]!).getByText("Exited 0")).toBeTruthy();
    // The collapsed row carries the reason a failed run failed.
    expect(screen.getByTestId("entry-reason").textContent).toBe("verifier exited 1");
    expect(within(trace).getByRole("button", { name: "Retry" })).toBeTruthy();
    // Every run can be replayed on the canvas it ran through.
    expect(within(trace).getByTestId("replay-link").getAttribute("href")).toBe(
      "/automations/auto-1?tab=build&run=run-bad",
    );
    expect(
      within(trace)
        .getByRole("link", { name: /open session/i })
        .getAttribute("href"),
    ).toBe("/sessions/se-finder-1");
  });

  it("a step row opens the details drawer with the rendered inputs and outputs", async () => {
    const listRuns = vi.fn<ListImpl>(() => ({
      runs: [brief("run-bad", "failed", 60)],
      filtered: [],
    }));
    const steps = [
      {
        blockId: "clone",
        attempt: 0,
        status: "succeeded",
        inputsJson: JSON.stringify({ commandTemplate: "git clone" }),
        outputsJson: JSON.stringify({ exit_status: 0, stdout: "Cloning into repo" }),
        startedAt: new Date(NOW - 60 * 60_000).toISOString(),
        endedAt: new Date(NOW - 60 * 60_000 + 9_000).toISOString(),
      },
    ];
    renderWithProviders(<ActivityTab automationId="auto-1" now={() => NOW} />, {
      transport: transportWith(listRuns, steps),
    });
    await userEvent.click(await screen.findByRole("button", { expanded: false }));
    await userEvent.click(await screen.findByRole("button", { name: "Details for clone" }));
    const drawer = await screen.findByRole("dialog");
    expect(within(drawer).getByText("run_command")).toBeTruthy();
    expect(within(drawer).getByText(/git clone/)).toBeTruthy();
    // stdout shows in the outputs JSON and again as its own block.
    expect(within(drawer).getAllByText(/Cloning into repo/).length).toBeGreaterThan(0);
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
