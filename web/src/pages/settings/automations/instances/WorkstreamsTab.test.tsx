import { beforeEach, describe, expect, it, vi } from "vitest";
import { toast } from "sonner";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Code, ConnectError, createRouterTransport } from "@connectrpc/connect";

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
  runNow?: (req: RunNowRequest) => { runId: string };
  onClose?: (id: string) => void;
  closeFails?: boolean;
}

function transportWith(impl: Impl) {
  return createRouterTransport((router) => {
    router.service(AutomationRunService, {
      listInstances: () => ({ instances: impl.instances ?? [] }),
      getInstance: (req) => ({
        instance: (impl.instances ?? []).find((i) => i.id === req.id),
        handles: impl.handles ?? [],
      }),
      closeInstance: (req) => {
        impl.onClose?.(req.id);
        if (impl.closeFails) throw new ConnectError("workstream not found", Code.NotFound);
        return { closed: true };
      },
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
  it("lists open workstreams by key, hides closed ones behind the toggle", async () => {
    renderTab({
      instances: [
        instance("ai_one", "project-ENG-1", "open"),
        instance("ai_two", "project-ENG-2", "closed", {
          closedAt: new Date(NOW - 60_000).toISOString(),
          closeReason: "shipped",
        }),
      ],
    });
    await waitFor(() => expect(screen.getByText("project-ENG-1")).toBeTruthy());
    expect(screen.queryByText("project-ENG-2")).toBeNull();
    expect(screen.getByText("1 open workstreams")).toBeTruthy();

    await userEvent.click(screen.getByRole("switch", { name: "show closed workstreams" }));
    await waitFor(() => expect(screen.getByText("project-ENG-2")).toBeTruthy());
    expect(screen.getByText(/closed · shipped/)).toBeTruthy();
  });

  it("expanding a row shows the input snapshot, handles, and a close button that closes", async () => {
    const closed: string[] = [];
    renderTab({
      instances: [instance("ai_one", "project-ENG-1", "open")],
      handles: [
        {
          handle: "slack:C1:1724.100",
          writtenBy: "run:post",
          createdAt: new Date(NOW).toISOString(),
        },
      ],
      onClose: (id) => closed.push(id),
    });
    await waitFor(() => expect(screen.getByText("project-ENG-1")).toBeTruthy());
    await userEvent.click(screen.getByText("project-ENG-1"));

    const detail = await screen.findByTestId("workstream-detail");
    expect(within(detail).getByText("ENG-1")).toBeTruthy();
    await waitFor(() => expect(within(detail).getByText("slack:C1:1724.100")).toBeTruthy());
    await userEvent.click(within(detail).getByRole("button", { name: "Close workstream" }));
    await waitFor(() => expect(closed).toEqual(["ai_one"]));
  });

  it("a failed close surfaces an error toast instead of failing silently", async () => {
    renderTab({
      instances: [instance("ai_one", "project-ENG-1", "open")],
      closeFails: true,
    });
    await waitFor(() => expect(screen.getByText("project-ENG-1")).toBeTruthy());
    await userEvent.click(screen.getByText("project-ENG-1"));
    const detail = await screen.findByTestId("workstream-detail");
    await userEvent.click(within(detail).getByRole("button", { name: "Close workstream" }));
    await waitFor(() =>
      expect(vi.mocked(toast.error)).toHaveBeenCalledWith(
        expect.stringContaining("workstream not found"),
      ),
    );
    expect(vi.mocked(toast.success)).not.toHaveBeenCalled();
  });

  it("kickoff posts RunNow with instance_key and the inputs snapshot", async () => {
    const requests: RunNowRequest[] = [];
    renderTab({
      instances: [],
      runNow: (req) => {
        requests.push(req);
        return { runId: "autorun:auto-1:main:i-ai_new:manual:1" };
      },
    });
    await waitFor(() => expect(screen.getByText(/No open workstreams/)).toBeTruthy());
    await userEvent.click(screen.getByRole("button", { name: /Kick off/ }));
    const form = await screen.findByTestId("kickoff-form");
    await userEvent.type(screen.getByRole("textbox", { name: "workstream key" }), "project-ENG-9");
    const projectField = within(form).getByRole("textbox", { name: "Project" });
    await userEvent.clear(projectField);
    await userEvent.type(projectField, "ENG-9");
    await userEvent.click(within(form).getByRole("button", { name: "Kick off" }));

    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toMatchObject({
      automationId: "auto-1",
      instanceKey: "project-ENG-9",
    });
    expect(JSON.parse(requests[0]!.instanceInputsJson ?? "{}")).toEqual({
      project: "ENG-9",
    });
    // Never the legacy field.
    expect(requests[0]!.inputsJson).toBeUndefined();
  });

  it("surfaces the recent-drops audit list", async () => {
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
    await waitFor(() => expect(screen.getByText("1 recent events did not fire")).toBeTruthy());
    await userEvent.click(screen.getByText("1 recent events did not fire"));
    const drops = screen.getByTestId("recent-drops");
    expect(within(drops).getByText("no workstream owns this")).toBeTruthy();
    expect(within(drops).getByText(/slack:C1:999/)).toBeTruthy();
  });
});
