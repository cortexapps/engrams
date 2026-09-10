import { beforeEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";

import {
  AutomationRunService,
  AutomationService,
  type RunNowRequest,
} from "@/gen/engram/app/v1/automation_pb";
import type { InputFieldSpec } from "@/lib/automation-inputs";
import { renderWithProviders } from "@/test-utils";

import { WorkstreamsTab } from "./WorkstreamsTab";

vi.mock("sonner", () => ({ toast: { success: vi.fn(), error: vi.fn() } }));

beforeEach(() => vi.clearAllMocks());

const NOW = Date.parse("2026-08-25T12:00:00Z");

function instance(
  id: string,
  key: string,
  status: "open" | "closed",
  extra: Record<string, unknown> = {},
) {
  return {
    id,
    automationId: "auto-1",
    key,
    status,
    inputsJson: JSON.stringify({ project: key.replace("project-", "") }),
    openedBy: "user:u-1",
    openedAt: new Date(NOW - 3_600_000).toISOString(),
    ...extra,
  };
}

interface Impl {
  instances?: ReturnType<typeof instance>[];
  handles?: Array<{ handle: string; writtenBy: string; createdAt: string }>;
  drops?: Array<{
    entrypointId: string;
    eventKey: string;
    reason: string;
    detail: string;
    droppedAt: string;
  }>;
  runNow?: (request: RunNowRequest) => { runId: string };
}

function transportWith(impl: Impl) {
  return createRouterTransport((router) => {
    router.service(AutomationRunService, {
      listInstances: () => ({ instances: impl.instances ?? [] }),
      getInstance: (request) => ({
        instance: (impl.instances ?? []).find((item) => item.id === request.id),
        handles: impl.handles ?? [],
      }),
      listRecentDrops: () => ({ drops: impl.drops ?? [] }),
      listRuns: () => ({ runs: [], filtered: [] }),
    });
    router.service(AutomationService, {
      runNow: impl.runNow ?? (() => ({ runId: "autorun:auto-1:x" })),
    });
  });
}

const SCHEMA: InputFieldSpec[] = [{ key: "project", label: "Project", type: "string" }];

function renderTab(impl: Impl) {
  return renderWithProviders(
    <WorkstreamsTab
      automationId="auto-1"
      inputsSchema={SCHEMA}
      defaultInputs={{ project: "default" }}
      now={() => NOW}
    />,
    { transport: transportWith(impl) },
  );
}

describe("WorkstreamsTab", () => {
  it("renders a human name and keeps the exact key, then shows closed workstreams on their tab", async () => {
    renderTab({
      instances: [
        instance("ai_one", "project-ENG_1", "open"),
        instance("ai_two", "project-ENG-2", "closed", {
          closedAt: new Date(NOW - 60_000).toISOString(),
          closeReason: "shipped",
        }),
      ],
    });

    expect(await screen.findByText("project ENG 1")).toBeTruthy();
    expect(screen.getByText("project-ENG_1")).toBeTruthy();
    expect(screen.queryByText("project ENG 2")).toBeNull();

    await userEvent.click(screen.getByRole("tab", { name: /Closed/ }));
    await waitFor(() => expect(screen.getByText("project ENG 2")).toBeTruthy());
  });

  it("shows owned places as chips and links rows to the detail page", async () => {
    renderTab({
      instances: [instance("ai_one", "project-ENG-1", "open")],
      handles: [
        {
          handle: "slack:C1:1724.100",
          writtenBy: "run:post",
          createdAt: new Date(NOW).toISOString(),
        },
      ],
    });

    const row = await screen.findByTestId("workstream-row");
    expect(await within(row).findByText("#C1 · thread")).toBeTruthy();
    expect(within(row).getByRole("link", { name: "project ENG 1" }).getAttribute("href")).toBe(
      "/automations/workstreams/ai_one",
    );
  });

  it("keeps close controls on the detail page instead of expanding rows", async () => {
    renderTab({ instances: [instance("ai_one", "project-ENG-1", "open")] });
    expect(await screen.findByText("project ENG 1")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Close workstream" })).toBeNull();
    expect(screen.queryByTestId("workstream-detail")).toBeNull();
  });

  it("opens a workstream with its name and input snapshot", async () => {
    const requests: RunNowRequest[] = [];
    renderTab({
      instances: [],
      runNow: (request) => {
        requests.push(request);
        return { runId: "autorun:auto-1:main:i-ai_new:manual:1" };
      },
    });
    expect(await screen.findByText(/No open workstreams/)).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Open a workstream" }));
    const form = await screen.findByTestId("open-workstream-form");
    await userEvent.type(screen.getByRole("textbox", { name: "workstream name" }), "ENG-9");
    const projectField = within(form).getByRole("textbox", { name: "Project" });
    await userEvent.clear(projectField);
    await userEvent.type(projectField, "ENG-9");
    await userEvent.click(within(form).getByRole("button", { name: "Open a workstream" }));

    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toMatchObject({ automationId: "auto-1", instanceKey: "ENG-9" });
    expect(JSON.parse(requests[0]!.instanceInputsJson ?? "{}")).toEqual({ project: "ENG-9" });
    expect(requests[0]!.inputsJson).toBeUndefined();
  });

  it("opens the next run for an already-open name without replacing its inputs", async () => {
    const requests: RunNowRequest[] = [];
    renderTab({
      instances: [instance("ai_one", "ENG-1", "open")],
      runNow: (request) => {
        requests.push(request);
        return { runId: "autorun:auto-1:join" };
      },
    });
    expect(await screen.findByText("ENG 1")).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Open a workstream" }));
    const form = await screen.findByTestId("open-workstream-form");
    await userEvent.type(screen.getByRole("textbox", { name: "workstream name" }), "ENG-1");

    expect((await screen.findByTestId("open-workstream-join-hint")).textContent).toBe(
      "ENG-1 is already open — this opens its next run",
    );
    expect(within(form).queryByRole("textbox", { name: "Project" })).toBeNull();
    await userEvent.click(within(form).getByRole("button", { name: "Open a workstream" }));

    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toMatchObject({ automationId: "auto-1", instanceKey: "ENG-1" });
    expect(requests[0]!.instanceInputsJson).toBeUndefined();
  });

  it("summarizes and expands recent events that did not fire", async () => {
    renderTab({
      instances: [instance("ai_one", "project-ENG-1", "open")],
      drops: [
        {
          entrypointId: "main",
          eventKey: "message",
          reason: "no_handle_match",
          detail: "slack:C1:999",
          droppedAt: new Date(NOW - 120_000).toISOString(),
        },
      ],
    });
    expect((await screen.findByTestId("recent-drops-summary")).textContent).toContain(
      "1 events did not fire in the last day",
    );
    await userEvent.click(screen.getByRole("button", { name: "Show them →" }));
    const drops = screen.getByTestId("recent-drops");
    expect(within(drops).getByText(/slack:C1:999/)).toBeTruthy();
  });
});
