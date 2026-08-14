import { describe, expect, test } from "vitest";

import type { IndexedEvent } from "@/lib/types";
import { currentToolLabel } from "./ConversationRail";

describe("currentToolLabel", () => {
  test("shows the latest active tool and clears it when the tool completes", () => {
    const started: IndexedEvent = {
      idx: 1,
      event: {
        type: "tool_call_started",
        run_id: "run-1",
        tool_call_id: "tool-1",
        tool_name: "mcp__engrams__spec_update_section",
        args_summary: null,
        at: "2026-08-13T10:00:00.000Z",
      },
    };
    const completed: IndexedEvent = {
      idx: 2,
      event: {
        type: "tool_call_completed",
        run_id: "run-1",
        tool_call_id: "tool-1",
        tool_name: "mcp__engrams__spec_update_section",
        ok: true,
        duration_ms: 10,
        result_summary: null,
        at: "2026-08-13T10:00:01.000Z",
      },
    };

    expect(currentToolLabel([started])).toBe("Updating the document");
    expect(currentToolLabel([started, completed])).toBeNull();
  });
});
