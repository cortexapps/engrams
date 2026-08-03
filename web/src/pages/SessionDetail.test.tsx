import { describe, expect, test } from "vitest";

import { hasLiveBrowserActivity } from "./SessionDetail";
import type { IndexedEvent } from "../lib/types";

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
