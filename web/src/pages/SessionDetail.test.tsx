import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

const testState = vi.hoisted(() => ({
  session: null as Record<string, unknown> | null,
  tasks: [] as Record<string, unknown>[],
  isMobile: false,
}));

vi.mock("@tanstack/react-router", () => ({
  useParams: () => ({ id: "session-1" }),
}));
vi.mock("../hooks/useSessions", () => ({
  useSession: () => ({ data: testState.session ?? undefined }),
}));
vi.mock("../hooks/useTasks", () => ({
  useTasks: () => ({ data: { tasks: testState.tasks } }),
  useTask: () => ({ data: undefined }),
}));
vi.mock("../hooks/useSessionEvents", () => ({
  useSessionEvents: () => ({ events: [], streamingText: "" }),
}));
vi.mock("../hooks/useDocumentTitle", () => ({
  useDocumentTitle: () => {},
}));
vi.mock("../hooks/use-mobile", () => ({
  useIsMobile: () => testState.isMobile,
}));
vi.mock("../auth/AuthProvider", () => ({
  useIsAdmin: () => false,
}));
vi.mock("../components/session-thread/SessionThread", () => ({
  SessionThread: () => <div data-testid="session-thread" />,
}));
vi.mock("../components/SessionDiagnostics", () => ({
  DiagnosticsPanel: () => null,
  DurabilityReadout: () => null,
  useDurabilitySummary: () => null,
}));
vi.mock("./sessions/DeleteSessionButton", () => ({
  DeleteSessionButton: () => <button type="button">Delete session</button>,
}));
vi.mock("@/components/ui/resizable", async () => {
  const React = await import("react");
  type PanelSize = { asPercentage: number; inPixels: number };
  const size = (pct: number): PanelSize => ({ asPercentage: pct, inPixels: pct * 10 });
  // v4 removed onCollapse/onExpand and takes the handle through a `panelRef`
  // PROP rather than `ref`. The mock mirrors that contract exactly, so a call
  // site that reverts to the v2 shape fails here instead of passing against a
  // library API that no longer exists.
  function MockResizablePanel({
    children,
    id,
    defaultSize,
    onResize,
    panelRef,
  }: {
    children?: React.ReactNode;
    id?: string;
    defaultSize?: number;
    onResize?: (s: PanelSize, id: string | undefined, prev: PanelSize | undefined) => void;
    panelRef?: React.Ref<{
      collapse: () => void;
      expand: () => void;
      resize: (s: number | string) => void;
    } | null>;
  }) {
    const open = defaultSize ?? 42;
    React.useImperativeHandle(panelRef, () => ({
      collapse: () => onResize?.(size(0), id, size(open)),
      expand: () => onResize?.(size(open), id, size(0)),
      resize: () => onResize?.(size(open), id, size(0)),
    }));
    return (
      <div data-testid={`resizable-panel-${id}`} data-default-size={defaultSize}>
        {children}
      </div>
    );
  }
  return {
    ResizablePanelGroup: ({ children }: { children?: React.ReactNode }) => (
      <div data-testid="resizable-panel-group">{children}</div>
    ),
    ResizablePanel: MockResizablePanel,
    ResizableHandle: () => <div data-testid="resizable-handle" />,
    percentSize: (value: number) => String(value),
  };
});
vi.mock("../components/WorkPane", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../components/WorkPane")>();
  return {
    ...actual,
    WorkPane: ({
      open,
      tab,
      selection,
      onCollapse,
    }: {
      open: boolean;
      tab: string;
      selection: unknown;
      onCollapse: () => void;
    }) => (
      <div
        data-testid="work-pane"
        data-open={String(open)}
        data-tab={tab}
        data-selection={JSON.stringify(selection)}
      >
        <button type="button" onClick={onCollapse}>
          Collapse pane
        </button>
      </div>
    ),
  };
});

import { hasLiveBrowserActivity, paneTabFromStored, SessionDetail } from "./SessionDetail";
import type { IndexedEvent } from "../lib/types";

beforeEach(() => {
  localStorage.clear();
  testState.isMobile = false;
  testState.session = {
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
  testState.tasks = [
    {
      id: "task-1",
      title: "Build the pane",
      titleIsCustom: true,
      harness: "task-harness",
      model: "task-model",
      sessions: [
        {
          sessionId: "session-1",
          profile: {
            id: "profile-1",
            name: "Builder",
            icon: "bot",
            archived: false,
            imageUri: "registry.example.com/engrams/base:latest",
            skills: [],
          },
        },
      ],
    },
  ];
});
afterEach(cleanup);

// The Browser pane auto-opens on agent browser activity (ADR 0097), but the
// SSE feed replays the whole durable log on every visit. These cases pin the
// boundary that keeps a re-opened session from popping the browser over
// history the agent produced hours ago.

const OPENED_AT = Date.parse("2026-08-03T12:00:00Z");

function browserActivity(idx: number, at: string): IndexedEvent {
  return {
    idx,
    event: {
      type: "browser_activity",
      run_id: "run-1",
      tool_call_id: `call-${idx}`,
      intent: "Open the dashboard",
      at,
    },
  };
}

describe("hasLiveBrowserActivity", () => {
  test("ignores replayed history from an earlier visit", () => {
    const events = [browserActivity(1, "2026-08-03T09:30:00Z")];
    expect(hasLiveBrowserActivity(events, OPENED_AT)).toBe(false);
  });

  test("reports activity that lands while the session is open", () => {
    const events = [
      browserActivity(1, "2026-08-03T09:30:00Z"),
      browserActivity(2, "2026-08-03T12:00:05Z"),
    ];
    expect(hasLiveBrowserActivity(events, OPENED_AT)).toBe(true);
  });

  test("ignores other event kinds", () => {
    const events: IndexedEvent[] = [
      {
        idx: 1,
        event: {
          type: "tool_call_started",
          run_id: "run-1",
          tool_call_id: "call-1",
          tool_name: "Bash",
          args_summary: null,
          at: "2026-08-03T12:00:05Z",
        },
      },
    ];
    expect(hasLiveBrowserActivity(events, OPENED_AT)).toBe(false);
  });

  test("treats an unparseable timestamp as not live", () => {
    expect(hasLiveBrowserActivity([browserActivity(1, "not-a-date")], OPENED_AT)).toBe(false);
  });

  test("is false on an empty log", () => {
    expect(hasLiveBrowserActivity([], OPENED_AT)).toBe(false);
  });
});

describe("paneTabFromStored", () => {
  test("keeps current pane view ids", () => {
    expect(paneTabFromStored("changes")).toBe("changes");
    expect(paneTabFromStored("shell")).toBe("shell");
  });

  test("maps retired and unknown pane ids to the overview", () => {
    for (const value of ["plan", "tasks", "side-effects", "events", null]) {
      expect(paneTabFromStored(value)).toBe("overview");
    }
  });
});

describe("SessionDetail workspace", () => {
  test("renders the slim masthead without an eyebrow, vitals, or desktop switcher", () => {
    render(<SessionDetail />);

    expect(screen.getByTestId("session-title").className).toContain("text-base");
    expect(screen.getByTestId("session-title").className).toContain("font-medium");
    expect(screen.getByTestId("session-status-glyph").getAttribute("aria-label")).toBe("active");
    expect(screen.queryByText(/^task$/i)).toBeNull();
    expect(screen.queryByTestId("session-status")).toBeNull();
    expect(screen.queryByTestId("masthead-pane-switcher")).toBeNull();
    expect(screen.queryByRole("button", { name: "Panel" })).toBeNull();
    expect(screen.queryByLabelText("Open work pane")).toBeNull();
  });

  test("opens the desktop pane at Overview with the preferred split", () => {
    render(<SessionDetail />);

    expect(screen.getByTestId("work-pane").getAttribute("data-open")).toBe("true");
    expect(screen.getByTestId("work-pane").getAttribute("data-tab")).toBe("overview");
    expect(screen.getByTestId("resizable-panel-transcript").getAttribute("data-default-size")).toBe(
      "58",
    );
    expect(screen.getByTestId("resizable-panel-workpane").getAttribute("data-default-size")).toBe(
      "42",
    );
  });

  // The switcher lives inside the pane, so a closed pane has no control of
  // its own — the masthead Panel button must appear as the way back in.
  test("offers the Panel button after a collapse and reopens from it", async () => {
    render(<SessionDetail />);

    expect(screen.queryByRole("button", { name: /Panel/ })).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Collapse pane" }));
    expect(screen.getByTestId("work-pane").getAttribute("data-open")).toBe("false");

    await userEvent.click(screen.getByRole("button", { name: /Panel/ }));
    expect(screen.getByTestId("work-pane").getAttribute("data-open")).toBe("true");
    expect(screen.queryByRole("button", { name: /Panel/ })).toBeNull();
  });

  // The orchestrator persists the EFFECTIVE selection on the task at create
  // time; the client shows what the task carries and never re-derives it.
  test("passes the task's persisted selection through to the pane", () => {
    render(<SessionDetail />);

    expect(JSON.parse(screen.getByTestId("work-pane").dataset.selection ?? "null")).toEqual({
      harness: "task-harness",
      model: "task-model",
    });
  });
});
