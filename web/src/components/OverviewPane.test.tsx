import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";
import { PrRefService } from "../gen/engram/app/v1/pr_ref_pb";
import { renderWithProviders } from "../test-utils";
import type { IndexedEvent } from "../events";
import type { Session } from "../lib/types";
import { OverviewPane } from "./OverviewPane";

vi.mock("./ports/ExposedPortsSection", () => ({
  ExposedPortsSection: () => <div data-testid="ports" />,
}));

afterEach(cleanup);

const session: Session = {
  id: "session-1",
  user_id: "user-1",
  status: "active",
  host_id: "host-1",
  sandbox_id: "sandbox-1",
  image: "registry.example.com/engrams/base:latest",
  mode: "agent",
  created_at: "2026-08-05T12:00:00.000Z",
  last_active_at: "2026-08-05T12:00:00.000Z",
};

function transportWithPrs(hasPr: boolean) {
  const requests: Array<{ taskId?: string; sessionId?: string }> = [];
  const transport = createRouterTransport((router) => {
    router.service(PrRefService, {
      listPrRefs: (request) => {
        requests.push({ taskId: request.taskId, sessionId: request.sessionId });
        return {
          prRefs: hasPr
            ? [
                {
                  id: "pr-1",
                  repo: "openai/engrams",
                  prNumber: 42,
                  authoringTaskId: request.taskId,
                  sessionId: "session-1",
                  title: "Build the overview pane",
                  url: "https://github.com/openai/engrams/pull/42",
                  headBranch: "overview",
                  baseBranch: "main",
                },
              ]
            : [],
        };
      },
    });
  });
  return { requests, transport };
}

function renderPane(
  events: IndexedEvent[] = [],
  hasPr = false,
  onShowChanges = vi.fn(),
  taskId: string | null = "task-1",
  sessionId = "session-1",
) {
  const { requests, transport } = transportWithPrs(hasPr);
  renderWithProviders(
    <OverviewPane
      sessionId={sessionId}
      taskId={taskId}
      session={session}
      events={events}
      profile={null}
      onShowChanges={onShowChanges}
    />,
    { transport },
  );
  return requests;
}

describe("OverviewPane", () => {
  test("renders pull request identity and its external link", async () => {
    const requests = renderPane([], true);

    await waitFor(() => expect(screen.getByText("openai/engrams#42")).toBeTruthy());
    expect(requests).toEqual([{ taskId: "task-1", sessionId: undefined }]);
    const link = screen.getByRole("link", { name: /Build the overview pane/ });
    expect(link.getAttribute("href")).toBe("https://github.com/openai/engrams/pull/42");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(screen.getByText("overview")).toBeTruthy();
    expect(screen.getByText("main")).toBeTruthy();
  });

  test("queries by session for an orphan session", async () => {
    const requests = renderPane([], true, vi.fn(), null, "orphan-session");

    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests).toEqual([{ taskId: undefined, sessionId: "orphan-session" }]);
  });

  test("renders the quiet pull request empty line", async () => {
    renderPane();
    await waitFor(() => expect(screen.getByText("No pull requests yet.")).toBeTruthy());
  });

  test("hides the changes summary when no file change events exist", () => {
    renderPane();
    expect(screen.queryByText(/files changed/)).toBeNull();
  });

  test("calls onShowChanges from the changes summary", async () => {
    const onShowChanges = vi.fn();
    const events: IndexedEvent[] = [
      {
        idx: 1,
        event: {
          type: "file_changed",
          run_id: "run-1",
          tool_call_id: "tool-1",
          path: "src/app.ts",
          change: { write: { content: "hello\n" } },
          at: "2026-08-05T12:01:00.000Z",
        },
      },
    ];
    renderPane(events, false, onShowChanges);

    await userEvent.click(await screen.findByRole("button", { name: /1 file changed/ }));
    expect(onShowChanges).toHaveBeenCalledOnce();
  });
});
