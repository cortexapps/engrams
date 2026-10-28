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

import { AgentsTab } from "./AgentsTab";

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
    <AgentsTab
      automationId="auto-1"
      inputsSchema={SCHEMA}
      defaultInputs={{ project: "default" }}
      now={() => NOW}
    />,
    { transport: transportWith(impl) },
  );
}

describe("AgentsTab", () => {
  it("renders a human name and keeps the exact key, then shows closed agents on their tab", async () => {
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

    const row = await screen.findByTestId("agent-row");
    // The place it owns is both its title and its chip.
    const link = await within(row).findByRole("link", { name: "#C1 · thread" });
    expect(link.getAttribute("href")).toBe("/automations/agents/ai_one");
    expect(within(row).getAllByText("#C1 · thread")).toHaveLength(2);
    expect(within(row).getByText("project-ENG-1")).toBeTruthy();
  });

  it("titles a row by the place it owns and closes it from the row", async () => {
    renderTab({
      instances: [instance("ai_one", "project-ENG-1", "open")],
      handles: [
        {
          handle: "github:acme/widgets#42",
          writtenBy: "run:open",
          createdAt: new Date(NOW).toISOString(),
        },
      ],
    });
    const row = await screen.findByTestId("agent-row");
    expect(await within(row).findByRole("link", { name: "acme/widgets#42" })).toBeTruthy();
    await userEvent.click(within(row).getByRole("button", { name: "Close" }));
    expect(await screen.findByRole("heading", { name: "Close acme/widgets#42?" })).toBeTruthy();
  });

  it("starts an agent with its name and input snapshot", async () => {
    const requests: RunNowRequest[] = [];
    renderTab({
      instances: [],
      runNow: (request) => {
        requests.push(request);
        return { runId: "autorun:auto-1:main:i-ai_new:manual:1" };
      },
    });
    expect(await screen.findByText(/No open agents/)).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Start an agent" }));
    const form = await screen.findByTestId("start-agent-form");
    await userEvent.type(screen.getByRole("textbox", { name: "agent name" }), "ENG-9");
    const projectField = within(form).getByRole("textbox", { name: "Project" });
    await userEvent.clear(projectField);
    await userEvent.type(projectField, "ENG-9");
    await userEvent.click(within(form).getByRole("button", { name: "Start an agent" }));

    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toMatchObject({ automationId: "auto-1", instanceKey: "ENG-9" });
    expect(JSON.parse(requests[0]!.instanceInputsJson ?? "{}")).toEqual({ project: "ENG-9" });
    expect(requests[0]!.inputsJson).toBeUndefined();
  });

  it("a standing agent (automation scope) starts under the engine's key, with no name asked", async () => {
    const requests: RunNowRequest[] = [];
    renderWithProviders(
      <AgentsTab
        automationId="auto-1"
        inputsSchema={SCHEMA}
        defaultInputs={{ project: "default" }}
        agentScope="automation"
        now={() => NOW}
      />,
      {
        transport: transportWith({
          instances: [],
          runNow: (request) => {
            requests.push(request);
            return { runId: "autorun:auto-1:main:i-ai_new:manual:1" };
          },
        }),
      },
    );
    expect(await screen.findByText(/No open agents/)).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Start an agent" }));
    const form = await screen.findByTestId("start-agent-form");
    expect(within(form).queryByRole("textbox", { name: "agent name" })).toBeNull();
    await userEvent.click(within(form).getByRole("button", { name: "Start the agent" }));
    await waitFor(() => expect(requests).toHaveLength(1));
    // The key the engine renders for an automation-scoped agent, never a typed one.
    expect(requests[0]).toMatchObject({ automationId: "auto-1", instanceKey: "agent" });
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
    await userEvent.click(screen.getByRole("button", { name: "Start an agent" }));
    const form = await screen.findByTestId("start-agent-form");
    await userEvent.type(screen.getByRole("textbox", { name: "agent name" }), "ENG-1");

    expect((await screen.findByTestId("start-agent-join-hint")).textContent).toBe(
      "ENG-1 is already open — this opens its next run",
    );
    expect(within(form).queryByRole("textbox", { name: "Project" })).toBeNull();
    await userEvent.click(within(form).getByRole("button", { name: "Start an agent" }));

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
        {
          entrypointId: "comment",
          eventKey: "issue_comment.created",
          reason: "render_failed",
          detail: "undefined variable: event.raw.pull_request",
          droppedAt: new Date(NOW - 60_000).toISOString(),
        },
      ],
    });
    const summary = (await screen.findByTestId("recent-drops-summary")).textContent;
    expect(summary).toContain("2 events did not fire in the last day");
    expect(summary).toContain("1 matched no open agent");
    expect(summary).toContain("1 had no key for the event");
    await userEvent.click(screen.getByRole("button", { name: "Show them →" }));
    const drops = screen.getByTestId("recent-drops");
    expect(within(drops).getByText(/slack:C1:999/)).toBeTruthy();
  });
});
