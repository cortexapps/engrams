import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

vi.mock("@tanstack/react-router", () => ({
  useParams: () => ({ id: "session-1" }),
}));
vi.mock("../hooks/useSessions", () => ({
  useSession: () => ({ data: undefined }),
}));
vi.mock("../hooks/useTasks", () => ({
  useTasks: () => ({ data: { tasks: [] } }),
}));
vi.mock("../hooks/useSessionEvents", () => ({
  useSessionEvents: () => ({ events: [], streamingText: "" }),
}));
vi.mock("../hooks/useDocumentTitle", () => ({
  useDocumentTitle: () => {},
}));
vi.mock("../hooks/use-mobile", () => ({
  useIsMobile: () => false,
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
  const ResizablePanel = React.forwardRef<
    { collapse: () => void; expand: () => void; resize: () => void },
    { children?: React.ReactNode }
  >(function MockResizablePanel({ children }, ref) {
    React.useImperativeHandle(ref, () => ({
      collapse: () => {},
      expand: () => {},
      resize: () => {},
    }));
    return <div data-testid="resizable-panel">{children}</div>;
  });
  return {
    ResizablePanelGroup: ({ children }: { children?: React.ReactNode }) => (
      <div data-testid="resizable-panel-group">{children}</div>
    ),
    ResizablePanel,
    ResizableHandle: () => <div data-testid="resizable-handle" />,
  };
});
vi.mock("../components/WorkPane", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../components/WorkPane")>();
  return {
    ...actual,
    WorkPane: ({ open, tab }: { open: boolean; tab: string }) => (
      <div data-testid="work-pane" data-open={String(open)} data-tab={tab} />
    ),
  };
});

import { hasLiveBrowserActivity, paneTabFromStored, SessionDetail } from "./SessionDetail";
import type { IndexedEvent } from "../lib/types";

beforeEach(() => localStorage.clear());
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

describe("SessionDetail pane switcher", () => {
  test("renders labeled masthead buttons and no edge rail", () => {
    render(<SessionDetail />);

    expect(screen.getByTestId("masthead-pane-overview").getAttribute("aria-pressed")).toBe("false");
    expect(screen.getByTestId("masthead-pane-shell").getAttribute("aria-pressed")).toBe("false");
    expect(screen.queryByLabelText("Open work pane")).toBeNull();
  });

  test("opens a selected view and collapses it when selected again", async () => {
    render(<SessionDetail />);
    const shell = screen.getByTestId("masthead-pane-shell");

    await userEvent.click(shell);
    expect(shell.getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByTestId("work-pane").getAttribute("data-open")).toBe("true");
    expect(screen.getByTestId("work-pane").getAttribute("data-tab")).toBe("shell");

    await userEvent.click(shell);
    expect(shell.getAttribute("aria-pressed")).toBe("false");
    expect(screen.getByTestId("work-pane").getAttribute("data-open")).toBe("false");
  });
});
