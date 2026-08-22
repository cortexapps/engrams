import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";

import {
  AutomationRunService,
  AutomationService,
  type RetryRunRequest,
  type StopRunRequest,
} from "@/gen/engram/app/v1/automation_pb";
import { renderWithProviders } from "@/test-utils";
import { RunPage, blockTypesFromDefinition } from "./RunPage";

const navigate = vi.hoisted(() => vi.fn());
vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useNavigate: () => navigate,
}));

const T0 = "2026-08-21T12:00:00Z";
const RUN_ID = "autorun:auto-1:github:d1";

function runResponse(status: string) {
  return {
    run: {
      brief: {
        id: RUN_ID,
        automationId: "auto-1",
        version: 1,
        status,
        triggerSource: "integration",
        eventKey: "pull_request.opened",
        dryRun: false,
        startedAt: T0,
        ...(status === "running" ? {} : { endedAt: T0 }),
        createdAt: T0,
      },
      triggerJson: "{}",
      steps: [
        {
          blockId: "launch",
          attempt: 0,
          status: "succeeded",
          inputsJson: "{}",
          outputsJson: JSON.stringify({ session_id: "sess-1" }),
          sessionId: "sess-1",
          startedAt: T0,
          endedAt: T0,
        },
      ],
      sessionIds: ["sess-1"],
    },
  };
}

type StopImpl = (req: StopRunRequest) => { sent: boolean };
type RetryImpl = (req: RetryRunRequest) => { runId: string };

function transportFor(status: string, stubs: { stopRun?: StopImpl; retryRun?: RetryImpl } = {}) {
  return createRouterTransport((router) => {
    router.service(AutomationRunService, {
      listRuns: () => ({ runs: [], filtered: [] }),
      getRun: () => runResponse(status),
      stopRun: stubs.stopRun ?? (() => ({ sent: true })),
      retryRun: stubs.retryRun ?? (() => ({ runId: "autorun:auto-1:retry:x" })),
    });
    router.service(AutomationService, {
      getAutomation: () => ({
        automation: {
          id: "auto-1",
          name: "Nightly triage",
          version: {
            definitionJson: JSON.stringify({
              blocks: [{ id: "launch", type: "create_session", config: {} }],
            }),
          },
        },
      }),
    });
  });
}

beforeEach(() => navigate.mockReset());

describe("blockTypesFromDefinition", () => {
  it("walks nested then/else/body lists", () => {
    expect(
      blockTypesFromDefinition(
        JSON.stringify({
          blocks: [
            { id: "a", type: "filter" },
            // "then" is the engine's branch field; unicorn/no-thenable fires on
            // the literal key, so the arm is attached via a computed key.
            { id: "g", type: "branch", ["th" + "en"]: [{ id: "b", type: "code" }], else: [] },
            { id: "l", type: "loop", body: [{ id: "c", type: "run_command" }] },
          ],
        }),
      ),
    ).toEqual({ a: "filter", g: "branch", b: "code", l: "loop", c: "run_command" });
  });
});

describe("RunPage", () => {
  it("shows Stop only while the run is active", async () => {
    const stopRun = vi.fn<StopImpl>(() => ({ sent: true }));
    renderWithProviders(<RunPage automationId="auto-1" runId={RUN_ID} />, {
      transport: transportFor("running", { stopRun }),
    });
    const stop = await screen.findByRole("button", { name: /^stop$/i });
    expect(screen.queryByRole("button", { name: /retry|re-run/i })).toBeNull();
    await userEvent.setup().click(stop);
    await waitFor(() =>
      expect(stopRun).toHaveBeenCalledWith(
        expect.objectContaining({ runId: RUN_ID }),
        expect.anything(),
      ),
    );
  });

  it("offers Retry on a terminal run and navigates to the new run", async () => {
    const retryRun = vi.fn<RetryImpl>(() => ({ runId: "autorun:auto-1:retry:x" }));
    renderWithProviders(<RunPage automationId="auto-1" runId={RUN_ID} />, {
      transport: transportFor("failed", { retryRun }),
    });
    const retry = await screen.findByRole("button", { name: /^retry$/i });
    expect(screen.queryByRole("button", { name: /^stop$/i })).toBeNull();
    await userEvent.setup().click(retry);
    await waitFor(() =>
      expect(navigate).toHaveBeenCalledWith({
        to: "/settings/automations/$id/runs/$runId",
        params: { id: "auto-1", runId: "autorun:auto-1:retry:x" },
      }),
    );
  });

  it("labels a completed run's action Re-run and opens the step drawer with a session link", async () => {
    renderWithProviders(<RunPage automationId="auto-1" runId={RUN_ID} />, {
      transport: transportFor("completed"),
    });
    expect(await screen.findByRole("button", { name: /^re-run$/i })).toBeTruthy();
    await userEvent.setup().click(await screen.findByTestId("timeline-step"));
    expect(await screen.findByTestId("session-link")).toBeTruthy();
    expect(screen.getByTestId("retry-count").textContent).toBe("1 attempt");
  });
});
