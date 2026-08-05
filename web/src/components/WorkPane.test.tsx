import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { IndexedEvent, SessionEvent } from "../events";
import { visibleTabCount, WorkPane, type PaneTabId, type WorkPaneProps } from "./WorkPane";

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

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

const baseProps: WorkPaneProps = {
  sessionId: "session-1",
  taskId: "task-1",
  session: undefined,
  events: [],
  profile: null,
  selection: null,
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
  test("shows primary views in priority order", () => {
    renderPane({ browserEnabled: true, ideEnabled: true });

    const tabs = ["overview", "browser", "ide"].map((id) => screen.getByTestId(`pane-tab-${id}`));
    expect(tabs.map((tab) => tab.getAttribute("aria-label"))).toEqual([
      "Overview",
      "Browser",
      "IDE",
    ]);
    expect(tabs[0].getAttribute("aria-pressed")).toBe("true");
  });

  test("promotes the active overflow view and demotes it after a primary switch", () => {
    const { rerender } = renderPane({ tab: "shell", browserEnabled: true });

    expect(screen.getByTestId("pane-tab-shell").getAttribute("aria-pressed")).toBe("true");

    rerender(<WorkPane {...baseProps} tab="browser" browserEnabled />);
    expect(screen.queryByTestId("pane-tab-shell")).toBeNull();
    expect(screen.getByTestId("pane-tab-browser").getAttribute("aria-pressed")).toBe("true");
  });

  test("swaps the active view into a fitting prefix", async () => {
    vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(
      function (this: HTMLElement) {
        return this.querySelector('[aria-hidden="true"]') ? 180 : 0;
      },
    );
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(
      function (this: HTMLElement) {
        return DOMRect.fromRect({ width: this.textContent?.trim() === "More" ? 48 : 60 });
      },
    );

    renderPane({ tab: "ide", browserEnabled: true, ideEnabled: true });

    expect(screen.getByTestId("pane-tab-overview")).toBeTruthy();
    expect(screen.queryByTestId("pane-tab-browser")).toBeNull();
    expect(screen.getByTestId("pane-tab-ide").getAttribute("aria-pressed")).toBe("true");
    await userEvent.click(screen.getByTestId("pane-more-menu"));
    expect(screen.getByRole("menuitem", { name: "Browser" })).toBeTruthy();
  });

  test("lists available overflow views in the More menu", async () => {
    renderPane({
      events: [fileChanged()],
      browserEnabled: true,
      ideEnabled: true,
      isAdmin: true,
    });
    await userEvent.click(screen.getByTestId("pane-more-menu"));

    for (const label of ["Changes", "Shell", "Diagnostics"]) {
      expect(screen.getByRole("menuitem", { name: label })).toBeTruthy();
    }
    expect(screen.queryByRole("menuitem", { name: "Browser" })).toBeNull();
    expect(screen.queryByRole("menuitem", { name: "IDE" })).toBeNull();
    const shellItem = screen.getByRole("menuitem", { name: "Shell" });
    expect(shellItem.className.split(" ")).toContain("px-2");
    expect(shellItem.className.split(" ")).not.toContain("pl-8");
  });

  test("omits unavailable Changes and Diagnostics menu items", async () => {
    renderPane();
    await userEvent.click(screen.getByTestId("pane-more-menu"));

    expect(screen.queryByRole("menuitem", { name: "Changes" })).toBeNull();
    expect(screen.queryByRole("menuitem", { name: "Diagnostics" })).toBeNull();
  });

  test("selecting a More menu item changes the view", async () => {
    const onTabChange = vi.fn();
    renderPane({ onTabChange });
    await userEvent.click(screen.getByTestId("pane-more-menu"));
    await userEvent.click(screen.getByRole("menuitem", { name: "Shell" }));

    expect(onTabChange).toHaveBeenCalledWith("shell");
  });

  test("uses the same priority header in the overlay", () => {
    renderPane({ variant: "overlay", browserEnabled: true });

    expect(screen.getByTestId("pane-tab-overview")).toBeTruthy();
    expect(screen.getByTestId("pane-tab-browser")).toBeTruthy();
    expect(screen.getByTestId("pane-more-menu")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Close pane" })).toBeTruthy();
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
    expect(screen.getByTestId("pane-tab-overview").getAttribute("aria-pressed")).toBe("true");
    expect(onTabChange).not.toHaveBeenCalled();
  });
});

describe("visibleTabCount", () => {
  test("returns every tab when they fit", () => {
    expect(visibleTabCount([60, 70, 50], 48, 2, 234)).toBe(3);
  });

  test("returns the fitting prefix", () => {
    expect(visibleTabCount([60, 70, 50], 48, 2, 184)).toBe(2);
  });

  test("keeps at least one tab visible", () => {
    expect(visibleTabCount([60, 70], 48, 2, 20)).toBe(1);
  });

  test("returns every tab for layout-free measurements", () => {
    expect(visibleTabCount([0, 0, 0], 0, 0, 0)).toBe(3);
  });
});
