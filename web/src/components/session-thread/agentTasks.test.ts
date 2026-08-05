// Tests for the agent task-list fold — the event-stream reconstruction of
// Claude Code's TaskCreate/TaskUpdate checklist that feeds the Tasks pane
// and the transcript's compact task rows.

import { describe, expect, test } from "vitest";
import { extractAgentTasks, sessionHasAgentTasks, taskCallDisplays } from "./agentTasks";
import type { IndexedEvent, SessionEvent } from "../../events";

const AT = "2026-08-05T12:00:00.000Z";

function indexed(events: SessionEvent[], rewoundIdxs: number[] = []): IndexedEvent[] {
  return events.map((event, idx) => ({ idx, event, rewound: rewoundIdxs.includes(idx) }));
}

function createStarted(
  toolCallId: string,
  args: Record<string, unknown> | string | null,
): SessionEvent {
  return {
    type: "tool_call_started",
    run_id: "r1",
    tool_call_id: toolCallId,
    tool_name: "TaskCreate",
    args_summary: typeof args === "string" || args === null ? args : JSON.stringify(args),
    at: AT,
  };
}

function updateStarted(toolCallId: string, args: Record<string, unknown>): SessionEvent {
  return {
    type: "tool_call_started",
    run_id: "r1",
    tool_call_id: toolCallId,
    tool_name: "TaskUpdate",
    args_summary: JSON.stringify(args),
    at: AT,
  };
}

function completed(toolCallId: string, ok: boolean, result: string | null): SessionEvent {
  return {
    type: "tool_call_completed",
    run_id: "r1",
    tool_call_id: toolCallId,
    tool_name: "",
    ok,
    duration_ms: 5,
    result_summary: result,
    at: AT,
  };
}

/** The observed harness wire shapes (2026-08-05):
 *  create result "Task #<id> created successfully: <subject>",
 *  update result "Updated task #<id> status". */
const created = (toolCallId: string, id: string, subject: string) =>
  completed(toolCallId, true, `Task #${id} created successfully: ${subject}`);
const updated = (toolCallId: string, id: string) =>
  completed(toolCallId, true, `Updated task #${id} status`);

describe("extractAgentTasks", () => {
  test("a completed TaskCreate yields a pending task with the guest-assigned id", () => {
    const tasks = extractAgentTasks(
      indexed([
        createStarted("t1", { subject: "Fix the bug", description: "…", activeForm: "Fixing" }),
        created("t1", "1", "Fix the bug"),
      ]),
    );
    expect(tasks).toEqual([
      {
        id: "1",
        subject: "Fix the bug",
        activeForm: "Fixing",
        status: "pending",
        createdBy: "t1",
        at: AT,
      },
    ]);
  });

  test("an in-flight TaskCreate (no completion yet) shows provisionally, id null", () => {
    const tasks = extractAgentTasks(indexed([createStarted("t1", { subject: "Fix the bug" })]));
    expect(tasks).toHaveLength(1);
    expect(tasks[0]).toMatchObject({ id: null, subject: "Fix the bug", status: "pending" });
  });

  test("a failed TaskCreate yields no task", () => {
    const tasks = extractAgentTasks(
      indexed([createStarted("t1", { subject: "Fix the bug" }), completed("t1", false, "boom")]),
    );
    expect(tasks).toHaveLength(0);
  });

  test("truncated args JSON falls back to the subject in the create result", () => {
    const tasks = extractAgentTasks(
      indexed([
        createStarted("t1", '{"subject":"Fix the bug","description":"very lo…[truncated]'),
        created("t1", "1", "Fix the bug"),
      ]),
    );
    expect(tasks[0]).toMatchObject({ id: "1", subject: "Fix the bug" });
  });

  test("TaskUpdate walks status pending → in_progress → completed", () => {
    const base = [
      createStarted("t1", { subject: "Fix the bug" }),
      created("t1", "1", "Fix the bug"),
    ];
    const inProgress = extractAgentTasks(
      indexed([
        ...base,
        updateStarted("u1", { taskId: "1", status: "in_progress" }),
        updated("u1", "1"),
      ]),
    );
    expect(inProgress[0]!.status).toBe("in_progress");
    const done = extractAgentTasks(
      indexed([
        ...base,
        updateStarted("u1", { taskId: "1", status: "in_progress" }),
        updated("u1", "1"),
        updateStarted("u2", { taskId: "1", status: "completed" }),
        updated("u2", "1"),
      ]),
    );
    expect(done[0]!.status).toBe("completed");
  });

  test("an in-flight TaskUpdate applies optimistically; a failed one is ignored", () => {
    const base = [
      createStarted("t1", { subject: "Fix the bug" }),
      created("t1", "1", "Fix the bug"),
    ];
    const optimistic = extractAgentTasks(
      indexed([...base, updateStarted("u1", { taskId: "1", status: "in_progress" })]),
    );
    expect(optimistic[0]!.status).toBe("in_progress");
    const failed = extractAgentTasks(
      indexed([
        ...base,
        updateStarted("u1", { taskId: "1", status: "in_progress" }),
        completed("u1", false, "task is stale"),
      ]),
    );
    expect(failed[0]!.status).toBe("pending");
  });

  test("status deleted removes the task from the live list", () => {
    const tasks = extractAgentTasks(
      indexed([
        createStarted("t1", { subject: "Fix the bug" }),
        created("t1", "1", "Fix the bug"),
        createStarted("t2", { subject: "Run tests" }),
        created("t2", "2", "Run tests"),
        updateStarted("u1", { taskId: "1", status: "deleted" }),
        updated("u1", "1"),
      ]),
    );
    expect(tasks.map((t) => t.subject)).toEqual(["Run tests"]);
  });

  test("subject and activeForm edits apply; an update to an unknown id is a no-op", () => {
    const tasks = extractAgentTasks(
      indexed([
        createStarted("t1", { subject: "Fix the bug" }),
        created("t1", "1", "Fix the bug"),
        updateStarted("u1", {
          taskId: "1",
          subject: "Fix the auth bug",
          activeForm: "Fixing auth",
        }),
        updated("u1", "1"),
        updateStarted("u2", { taskId: "99", status: "completed" }),
        completed("u2", false, "not found"),
      ]),
    );
    expect(tasks[0]).toMatchObject({
      subject: "Fix the auth bug",
      activeForm: "Fixing auth",
      status: "pending",
    });
  });

  test("a re-fired create with the same tool_call_id yields one task", () => {
    const tasks = extractAgentTasks(
      indexed([
        createStarted("t1", { subject: "Fix the bug" }),
        createStarted("t1", { subject: "Fix the bug" }),
        created("t1", "1", "Fix the bug"),
      ]),
    );
    expect(tasks).toHaveLength(1);
  });

  test("rewound events are excluded — the guest state rolled back with them", () => {
    const tasks = extractAgentTasks(
      indexed(
        [
          createStarted("t1", { subject: "Fix the bug" }),
          created("t1", "1", "Fix the bug"),
          createStarted("t2", { subject: "Rolled back" }),
          created("t2", "2", "Rolled back"),
        ],
        [2, 3],
      ),
    );
    expect(tasks.map((t) => t.subject)).toEqual(["Fix the bug"]);
  });

  test("id resolution is order-independent: the create result may precede the start in the array", () => {
    const tasks = extractAgentTasks(
      indexed([created("t1", "1", "Fix the bug"), createStarted("t1", { subject: "Fix the bug" })]),
    );
    expect(tasks[0]).toMatchObject({ id: "1" });
  });
});

describe("taskCallDisplays", () => {
  test("an update row resolves its target's subject as of that call", () => {
    const displays = taskCallDisplays(
      indexed([
        createStarted("t1", { subject: "Fix the bug" }),
        created("t1", "1", "Fix the bug"),
        updateStarted("u1", { taskId: "1", status: "in_progress" }),
        updated("u1", "1"),
        updateStarted("u2", { taskId: "1", subject: "Fix the auth bug" }),
        updated("u2", "1"),
        updateStarted("u3", { taskId: "1", status: "completed" }),
        updated("u3", "1"),
      ]),
    );
    expect(displays.get("t1")).toEqual({ action: "create", subject: "Fix the bug", status: null });
    expect(displays.get("u1")).toEqual({
      action: "update",
      subject: "Fix the bug",
      status: "in_progress",
    });
    expect(displays.get("u2")).toEqual({
      action: "update",
      subject: "Fix the auth bug",
      status: null,
    });
    expect(displays.get("u3")).toEqual({
      action: "update",
      subject: "Fix the auth bug",
      status: "completed",
    });
  });

  test("a delete row still names its target after the task is gone", () => {
    const displays = taskCallDisplays(
      indexed([
        createStarted("t1", { subject: "Fix the bug" }),
        created("t1", "1", "Fix the bug"),
        updateStarted("u1", { taskId: "1", status: "deleted" }),
        updated("u1", "1"),
      ]),
    );
    expect(displays.get("u1")).toEqual({
      action: "update",
      subject: "Fix the bug",
      status: "deleted",
    });
  });
});

describe("sessionHasAgentTasks", () => {
  test("true once any TaskCreate start appears; false otherwise", () => {
    expect(sessionHasAgentTasks(indexed([createStarted("t1", { subject: "x" })]))).toBe(true);
    expect(
      sessionHasAgentTasks(indexed([updateStarted("u1", { taskId: "1", status: "completed" })])),
    ).toBe(false);
    expect(sessionHasAgentTasks([])).toBe(false);
  });
});
