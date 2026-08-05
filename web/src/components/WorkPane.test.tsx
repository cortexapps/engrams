import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { IndexedEvent, SessionEvent } from "../events";
import { stripLayout, WorkPane, type PaneTabId, type WorkPaneProps } from "./WorkPane";

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

  // jsdom has no layout, so the zero-width guard shows every view — which is
  // also the wide-strip contract: room for everything → everything is a tab
  // and the More trigger never renders.
  test("surfaces every available view as a labeled tab when the strip has room", () => {
    renderPane({
      events: [fileChanged()],
      browserEnabled: true,
      ideEnabled: true,
      isAdmin: true,
    });

    for (const id of ["overview", "browser", "ide", "changes", "shell", "diagnostics"]) {
      expect(screen.getByTestId(`pane-tab-${id}`)).toBeTruthy();
    }
    expect(screen.queryByTestId("pane-more-menu")).toBeNull();
  });

  test("omits unavailable views entirely", () => {
    renderPane();

    expect(screen.queryByTestId("pane-tab-changes")).toBeNull();
    expect(screen.queryByTestId("pane-tab-browser")).toBeNull();
    expect(screen.queryByTestId("pane-tab-ide")).toBeNull();
    expect(screen.queryByTestId("pane-tab-diagnostics")).toBeNull();
    expect(screen.queryByTestId("pane-more-menu")).toBeNull();
  });

  // Labeled measurement buttons carry a text span; icon-only measures and the
  // More trigger don't — the mock keys widths off that structural difference.
  function mockNarrowStrip(width: number) {
    vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(
      function (this: HTMLElement) {
        return this.querySelector('[aria-hidden="true"]') ? width : 0;
      },
    );
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(
      function (this: HTMLElement) {
        return DOMRect.fromRect({ width: this.querySelector("span") ? 60 : 30 });
      },
    );
  }

  test("drops labels when the labeled set stops fitting", () => {
    // 3 views: labeled needs 184px, icon-only needs 94px — 120px → icons, no More.
    mockNarrowStrip(120);
    renderPane({ browserEnabled: true, ideEnabled: true });

    for (const id of ["overview", "browser", "ide"]) {
      const tabEl = screen.getByTestId(`pane-tab-${id}`);
      expect(tabEl.querySelector("span")).toBeNull();
      expect(tabEl.getAttribute("aria-label")).toBeTruthy();
    }
    expect(screen.queryByTestId("pane-more-menu")).toBeNull();
  });

  test("collapses the non-fitting suffix into the More menu", async () => {
    // 80px fits one 30px icon tab plus the reserved More trigger.
    mockNarrowStrip(80);
    renderPane({ browserEnabled: true, ideEnabled: true });

    expect(screen.getByTestId("pane-tab-overview")).toBeTruthy();
    expect(screen.queryByTestId("pane-tab-browser")).toBeNull();
    await userEvent.click(screen.getByTestId("pane-more-menu"));
    for (const label of ["Browser", "IDE", "Shell"]) {
      expect(screen.getByRole("menuitem", { name: label })).toBeTruthy();
    }
    const shellItem = screen.getByRole("menuitem", { name: "Shell" });
    expect(shellItem.className.split(" ")).toContain("px-2");
    expect(shellItem.className.split(" ")).not.toContain("pl-8");
  });

  test("swaps the active view into a fitting prefix", async () => {
    mockNarrowStrip(80);
    renderPane({ tab: "ide", browserEnabled: true, ideEnabled: true });

    expect(screen.getByTestId("pane-tab-overview")).toBeTruthy();
    expect(screen.queryByTestId("pane-tab-browser")).toBeNull();
    expect(screen.getByTestId("pane-tab-ide").getAttribute("aria-pressed")).toBe("true");
    await userEvent.click(screen.getByTestId("pane-more-menu"));
    expect(screen.getByRole("menuitem", { name: "Browser" })).toBeTruthy();
  });

  test("selecting a More menu item changes the view", async () => {
    // 40px: only Overview fits even icon-only; Shell lands in the menu.
    mockNarrowStrip(40);
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
    expect(screen.getByTestId("pane-tab-shell")).toBeTruthy();
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

describe("stripLayout", () => {
  const labeled = [60, 70, 50];
  const icons = [30, 30, 30];

  test("keeps labels when every labeled tab fits", () => {
    expect(stripLayout(labeled, icons, 30, 2, 184)).toEqual({ count: 3, iconOnly: false });
  });

  test("drops to icons when labels overflow but icons fit", () => {
    expect(stripLayout(labeled, icons, 30, 2, 100)).toEqual({ count: 3, iconOnly: true });
  });

  test("collapses the icon suffix behind the More trigger", () => {
    // 70px: More(30) + gap+icon(32) fits once; the second icon would need 134.
    expect(stripLayout(labeled, icons, 30, 2, 70)).toEqual({ count: 1, iconOnly: true });
  });

  test("keeps at least one tab visible", () => {
    expect(stripLayout(labeled, icons, 30, 2, 10)).toEqual({ count: 1, iconOnly: true });
  });

  test("returns every labeled tab for layout-free measurements", () => {
    expect(stripLayout([0, 0, 0], [0, 0, 0], 0, 0, 0)).toEqual({ count: 3, iconOnly: false });
  });
});
