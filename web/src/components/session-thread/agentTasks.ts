import type { IndexedEvent } from "../../events";

// The agent's own task list (Claude Code's TaskCreate/TaskUpdate tools),
// reconstructed from the session event stream. The tools are guest-native, so
// the platform never sees the list directly — only the tool calls. Folding
// tool_call_started args (JSON, 1 KB cap) + tool_call_completed results gives
// the same checklist Claude Code renders in its own UI.
//
// Correlation quirk: the harness assigns the task id INSIDE the guest, so a
// TaskCreate's id only appears in its RESULT text ("Task #3 created
// successfully: <subject>"). The fold parses it out; a create whose result
// has not landed yet is provisional (id null) and updates cannot target it
// until the result arrives — the next fold pass resolves it.

export type AgentTaskStatus = "pending" | "in_progress" | "completed";

export interface AgentTask {
  /** Guest-assigned id parsed from the create result; null while in flight. */
  id: string | null;
  subject: string;
  /** Present-continuous label the agent supplied for the in_progress spinner. */
  activeForm: string | null;
  status: AgentTaskStatus;
  /** tool_call_id of the creating TaskCreate — the stable render key. */
  createdBy: string;
  at: string;
}

/** Tool names as the Claude harness emits them (guest-native, no MCP prefix). */
export const TASK_CREATE = "TaskCreate";
export const TASK_UPDATE = "TaskUpdate";

export function isTaskToolName(name: string): boolean {
  return name === TASK_CREATE || name === TASK_UPDATE;
}

function parseArgs(argsSummary: string | null): Record<string, unknown> {
  if (!argsSummary) return {};
  try {
    const o = JSON.parse(argsSummary) as unknown;
    return o && typeof o === "object" ? (o as Record<string, unknown>) : {};
  } catch {
    // A >1 KB description truncates the args JSON mid-string — recover what
    // the regex fallbacks below can, never break the pane.
    return {};
  }
}

function str(v: unknown): string | null {
  return typeof v === "string" && v.length > 0 ? v : null;
}

/** "Task #3 created successfully: <subject>" → "3". */
function idFromCreateResult(result: string): string | null {
  const m = /^Task #(\S+) created/.exec(result);
  return m ? m[1]! : null;
}

/** "Task #3 created successfully: <subject>" → "<subject>" — the fallback
 *  when truncated args JSON failed to parse. */
function subjectFromCreateResult(result: string): string | null {
  const m = /^Task #\S+ created successfully: (.+)$/s.exec(result);
  return m ? m[1]! : null;
}

/** How the transcript should label one TaskCreate/TaskUpdate call — the args
 *  of the synthetic `engram.task` part buildMessages swaps in. `subject` is
 *  resolved as of THAT call (an update names its target even though the
 *  update's own args carry only the id). */
export interface TaskCallDisplay {
  action: "create" | "update";
  subject: string | null;
  status: AgentTaskStatus | "deleted" | null;
}

interface Fold {
  /** Live tasks in creation order (deleted ones removed). */
  tasks: AgentTask[];
  /** tool_call_id → transcript label for each task tool call. */
  displays: Map<string, TaskCallDisplay>;
}

function fold(events: IndexedEvent[]): Fold {
  // Pre-scan completions so id resolution is order-independent (the create's
  // result may land in a later SSE batch than its start).
  const completions = new Map<string, { ok: boolean; result: string | null }>();
  for (const { event, rewound } of events) {
    if (rewound) continue;
    if (event.type === "tool_call_completed") {
      completions.set(event.tool_call_id, { ok: event.ok, result: event.result_summary });
    }
  }

  const tasks: AgentTask[] = [];
  const byId = new Map<string, AgentTask>();
  // Latest subject per id, INCLUDING deleted tasks — an update row must name
  // its target even after the target is gone from the live list.
  const subjectById = new Map<string, string>();
  const displays = new Map<string, TaskCallDisplay>();
  const seenCreates = new Set<string>();

  const parseStatus = (v: unknown): AgentTaskStatus | "deleted" | null =>
    v === "pending" || v === "in_progress" || v === "completed" || v === "deleted" ? v : null;

  for (const { event: ev, rewound } of events) {
    // A rung-1 recovery rewound the guest too — tasks from the rolled-back
    // span no longer exist in the harness's list.
    if (rewound) continue;
    if (ev.type !== "tool_call_started") continue;

    if (ev.tool_name === TASK_CREATE) {
      const done = completions.get(ev.tool_call_id);
      const args = parseArgs(ev.args_summary);
      const subject =
        str(args.subject) ?? (done?.result ? subjectFromCreateResult(done.result) : null);
      displays.set(ev.tool_call_id, { action: "create", subject, status: null });
      // A resume can re-fire a tool call with the same id — one task per call.
      if (seenCreates.has(ev.tool_call_id)) continue;
      seenCreates.add(ev.tool_call_id);
      if (done && !done.ok) continue; // failed create → no task exists
      if (!subject) continue;
      const id = done?.result ? idFromCreateResult(done.result) : null;
      const task: AgentTask = {
        id,
        subject,
        activeForm: str(args.activeForm),
        status: "pending",
        createdBy: ev.tool_call_id,
        at: ev.at,
      };
      tasks.push(task);
      if (id) {
        byId.set(id, task);
        subjectById.set(id, subject);
      }
    } else if (ev.tool_name === TASK_UPDATE) {
      const args = parseArgs(ev.args_summary);
      const taskId = str(args.taskId);
      const status = parseStatus(args.status);
      displays.set(ev.tool_call_id, {
        action: "update",
        // As-of-this-call resolution: the subject the target had here, with
        // this call's own rename applied.
        subject: str(args.subject) ?? (taskId ? (subjectById.get(taskId) ?? null) : null),
        status,
      });
      const done = completions.get(ev.tool_call_id);
      // A failed update (stale/unknown id) changed nothing guest-side. An
      // update still in flight applies optimistically — the harness resolves
      // it within the same turn, and the spinner should not lag it.
      if (done && !done.ok) continue;
      if (!taskId) continue;
      const task = byId.get(taskId);
      if (!task) continue;
      const subject = str(args.subject);
      if (subject) {
        task.subject = subject;
        subjectById.set(taskId, subject);
      }
      const activeForm = str(args.activeForm);
      if (activeForm) task.activeForm = activeForm;
      if (status === "deleted") {
        byId.delete(taskId);
        const i = tasks.indexOf(task);
        if (i !== -1) tasks.splice(i, 1);
      } else if (status !== null) {
        task.status = status;
      }
    }
  }

  return { tasks, displays };
}

/** Gates the Tasks tab: has the agent ever created a task in this session?
 *  Deliberately cheaper than the full fold — the tab shows even when every
 *  task was later deleted (the pane then explains the empty list). */
export function sessionHasAgentTasks(events: IndexedEvent[]): boolean {
  return events.some(
    ({ event }) => event.type === "tool_call_started" && event.tool_name === TASK_CREATE,
  );
}

/** The agent's live task list in creation order (deleted tasks removed). */
export function extractAgentTasks(events: IndexedEvent[]): AgentTask[] {
  return fold(events).tasks;
}

/** tool_call_id → transcript label for every TaskCreate/TaskUpdate call —
 *  feeds buildMessages' synthetic `engram.task` part swap. */
export function taskCallDisplays(events: IndexedEvent[]): Map<string, TaskCallDisplay> {
  return fold(events).displays;
}
