import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { IndexedEvent, SessionEvent } from "../events";
import { WorkPane, type PaneTabId, type WorkPaneProps } from "./WorkPane";

vi.mock("./OverviewPane", () => ({
  OverviewPane: () => <div data-testid="overview-pane">Overview content</div>,
}));
vi.mock("./ChangesPane", () => ({
  ChangesPane: () => <div data-testid="changes-pane">Changes content</div>,
}));
vi.mock("./TerminalPane", () => ({
  TerminalPane: () => <div data-testid="shell-pane">Shell content</div>,
}));
vi.mock("./BrowserPane", () => ({
  BrowserPane: () => <div data-testid="browser-pane">Browser content</div>,
}));
vi.mock("./IdePane", () => ({
  IdePane: () => <div data-testid="ide-pane">IDE content</div>,
}));
vi.mock("./SessionDiagnostics", () => ({
  DiagnosticsPanel: () => <div data-testid="diagnostics-pane">Diagnostics content</div>,
}));

afterEach(cleanup);

const baseProps: WorkPaneProps = {
  sessionId: "session-1",
  taskId: "task-1",
  session: undefined,
  events: [],
  profile: null,
  isAdmin: false,
  open: true,
  tab: "overview",
  onTabChange: () => {},
  browserEnabled: false,
  ideEnabled: false,
  onCollapse: () => {},
};

function renderPane(props: Partial<WorkPaneProps> = {}) {
  return render(<WorkPane {...baseProps} {...props} />);
}

function fileChanged(): IndexedEvent {
  return {
    idx: 1,
    event: {
      type: "file_changed",
      run_id: "run-1",
      tool_call_id: "change-1",
      path: "src/app.ts",
      change: { write: { content: "hello\n" } },
      at: "2026-08-05T12:00:00.000Z",
    },
  };
}

function planEvent(): IndexedEvent {
  const event: SessionEvent = {
    type: "tool_call_requested",
    run_id: "run-1",
    tool_call_id: "plan-1",
    name: "exit_plan_mode",
    args_json: JSON.stringify({ plan: "# Plan" }),
    at: "2026-08-05T12:00:00.000Z",
  };
  return { idx: 1, event };
}

describe("WorkPane", () => {
  test("shows a static current-view label in the panel header", () => {
    renderPane({ tab: "shell" });

    expect(screen.getByTestId("pane-current-view").textContent).toContain("Shell");
    expect(screen.queryByTestId("pane-view-menu")).toBeNull();
  });

  test("shows the labeled view menu in an overlay header", () => {
    renderPane({ variant: "overlay" });

    expect(screen.getByTestId("pane-view-menu").textContent).toContain("Overview");
    expect(screen.queryByTestId("pane-current-view")).toBeNull();
  });

  test("lists every available view in the overlay menu", async () => {
    renderPane({
      variant: "overlay",
      events: [fileChanged()],
      browserEnabled: true,
      ideEnabled: true,
      isAdmin: true,
    });
    await userEvent.click(screen.getByTestId("pane-view-menu"));

    for (const label of ["Overview", "Changes", "Shell", "Browser", "IDE", "Diagnostics"]) {
      expect(screen.getByRole("menuitemcheckbox", { name: label })).toBeTruthy();
    }
  });

  test("omits unavailable Changes and Diagnostics menu items", async () => {
    renderPane({ variant: "overlay" });
    await userEvent.click(screen.getByTestId("pane-view-menu"));

    expect(screen.queryByRole("menuitemcheckbox", { name: "Changes" })).toBeNull();
    expect(screen.queryByRole("menuitemcheckbox", { name: "Diagnostics" })).toBeNull();
  });

  test("selecting an overlay menu item changes the view", async () => {
    const onTabChange = vi.fn();
    renderPane({ variant: "overlay", onTabChange });
    await userEvent.click(screen.getByTestId("pane-view-menu"));
    await userEvent.click(screen.getByRole("menuitemcheckbox", { name: "Shell" }));

    expect(onTabChange).toHaveBeenCalledWith("shell");
  });

  test("keeps the work dock visible on every open view", () => {
    const events = [planEvent()];
    const { rerender } = renderPane({ events });
    expect(screen.getByText("rev 1")).toBeTruthy();

    rerender(<WorkPane {...baseProps} events={events} tab="shell" />);
    expect(screen.getByText("rev 1")).toBeTruthy();
  });

  test("renders the overview for a stale controlled tab without changing it", () => {
    const onTabChange = vi.fn();
    renderPane({ tab: "tasks" as PaneTabId, onTabChange });

    expect(screen.getByTestId("overview-pane")).toBeTruthy();
    expect(screen.getByTestId("pane-current-view").textContent).toContain("Overview");
    expect(onTabChange).not.toHaveBeenCalled();
  });
});
