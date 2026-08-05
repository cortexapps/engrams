import { useMemo } from "react";
import { CircleCheckIcon, CircleDashedIcon, CircleDotIcon } from "lucide-react";
import { cn } from "@/lib/utils";
import {
  extractAgentTasks,
  type AgentTask,
  type AgentTaskStatus,
} from "@/components/session-thread/agentTasks";
import type { IndexedEvent } from "../events";

// The agent's task checklist (Claude Code's TaskCreate/TaskUpdate), rebuilt
// from the event stream — the side-panel counterpart to Claude Code's own
// todo display. Read-only: the guest owns the list; this pane only mirrors it.

const STATUS_ORDER: Record<AgentTaskStatus, number> = {
  in_progress: 0,
  pending: 1,
  completed: 2,
};

function StatusIcon({ status }: { status: AgentTaskStatus }) {
  switch (status) {
    case "in_progress":
      return <CircleDotIcon aria-label="In progress" className="size-4 shrink-0 text-primary" />;
    case "completed":
      return (
        <CircleCheckIcon
          aria-label="Completed"
          className="size-4 shrink-0 text-emerald-600 dark:text-emerald-400"
        />
      );
    default:
      return (
        <CircleDashedIcon aria-label="Pending" className="size-4 shrink-0 text-muted-foreground" />
      );
  }
}

function TaskRow({ task }: { task: AgentTask }) {
  const inProgress = task.status === "in_progress";
  const completed = task.status === "completed";
  return (
    <li className="flex items-start gap-2.5 px-4 py-2" data-testid="agent-task-row">
      <span className="mt-0.5">
        <StatusIcon status={task.status} />
      </span>
      <span className="min-w-0 flex-1">
        <span
          className={cn(
            "block text-sm",
            completed ? "text-muted-foreground line-through" : "text-foreground",
          )}
        >
          {task.subject}
        </span>
        {inProgress && task.activeForm && (
          <span className="block animate-pulse text-xs text-muted-foreground italic">
            {task.activeForm}…
          </span>
        )}
      </span>
    </li>
  );
}

export function TasksPane({ events }: { events: IndexedEvent[] }) {
  const tasks = useMemo(() => extractAgentTasks(events), [events]);
  // Active work floats to the top; ties keep creation order (stable sort).
  const ordered = useMemo(
    () => [...tasks].sort((a, b) => STATUS_ORDER[a.status] - STATUS_ORDER[b.status]),
    [tasks],
  );

  if (tasks.length === 0) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-muted-foreground italic">
        No tasks yet.
      </div>
    );
  }

  const done = tasks.filter((t) => t.status === "completed").length;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex shrink-0 items-center justify-between border-b px-4 py-2 text-xs text-muted-foreground">
        <span>Agent task list</span>
        <span data-testid="agent-task-progress">
          {done} of {tasks.length} completed
        </span>
      </div>
      <ul className="min-h-0 flex-1 divide-y divide-border/60 overflow-auto">
        {ordered.map((task) => (
          <TaskRow key={task.createdBy} task={task} />
        ))}
      </ul>
    </div>
  );
}
