import { Link } from "@tanstack/react-router";
import { GitBranch } from "lucide-react";

import type { Task } from "../gen/engram/app/v1/task_pb";
import { StatusDot, type StatusTone } from "@/components/status-dot";
import { cn } from "@/lib/utils";

function taskSessionId(task: Task): string | undefined {
  return (
    task.sessions.find((session) => session.role === "primary")?.sessionId ??
    task.sessions[0]?.sessionId
  );
}

function taskName(task: Task, root: boolean): string {
  if (!root && task.localTaskName) return task.localTaskName;
  return task.title ?? task.localTaskName ?? task.id;
}

function stateTone(state: string): StatusTone {
  switch (state) {
    case "working":
      return "active";
    case "failed":
      return "critical";
    case "open":
    case "awaiting_review":
      return "caution";
    default:
      return "muted";
  }
}

function stateLabel(state: string): string {
  const label = state.replaceAll("_", " ");
  return label.charAt(0).toUpperCase() + label.slice(1);
}

export function taskTreeRows(rootTask: Task): Array<{ task: Task; depth: number }> {
  const descendants = [...rootTask.descendants].sort((left, right) =>
    (left.canonicalTaskName ?? "").localeCompare(right.canonicalTaskName ?? ""),
  );
  return [
    { task: rootTask, depth: 0 },
    ...descendants.map((task) => ({
      task,
      depth: task.canonicalTaskName?.split("/").length ?? 1,
    })),
  ];
}

export function TaskTreeNavigation({
  rootTask,
  currentSessionId,
}: {
  rootTask: Task | null | undefined;
  currentSessionId: string;
}) {
  if (!rootTask || rootTask.descendants.length === 0) return null;
  const rows = taskTreeRows(rootTask);

  return (
    <section className="overflow-hidden rounded-lg border bg-card" aria-labelledby="subtasks-title">
      <div className="flex items-center gap-2 border-b px-3 py-2.5">
        <GitBranch className="size-4 text-muted-foreground" />
        <h2 id="subtasks-title" className="flex-1 text-sm font-semibold">
          Subtasks
        </h2>
        <span className="font-mono text-2xs text-muted-foreground">
          {rootTask.descendants.length} {rootTask.descendants.length === 1 ? "subtask" : "subtasks"}
        </span>
      </div>
      <div className="p-1.5" role="tree" aria-label="Subtasks">
        {rows.map(({ task, depth }) => {
          const sessionId = taskSessionId(task);
          const current = sessionId === currentSessionId;
          const root = depth === 0;
          const content = (
            <>
              <span
                className={cn(
                  "relative flex size-4 shrink-0 items-center justify-center",
                  depth > 0 &&
                    "before:absolute before:-left-3 before:h-px before:w-3 before:bg-border",
                )}
                aria-hidden
              >
                <StatusDot tone={stateTone(task.status)} size={6} />
              </span>
              <span className="min-w-0 flex-1 truncate text-sm" title={taskName(task, root)}>
                {taskName(task, root)}
              </span>
              <span className="shrink-0 text-2xs text-muted-foreground">
                {root ? "Root" : stateLabel(task.status)}
              </span>
            </>
          );
          const className = cn(
            "relative flex min-h-8 items-center gap-2 rounded-md px-2 transition-colors",
            depth > 0 &&
              "before:absolute before:-left-1 before:bottom-0 before:top-0 before:w-px before:bg-border",
            current
              ? "bg-accent font-medium text-foreground"
              : "text-muted-foreground hover:bg-accent/70 hover:text-foreground",
          );
          const style = { marginInlineStart: `${depth * 0.875}rem` };

          return sessionId ? (
            <Link
              key={task.id}
              to="/sessions/$id"
              params={{ id: sessionId }}
              role="treeitem"
              aria-level={depth + 1}
              aria-current={current ? "page" : undefined}
              className={className}
              style={style}
            >
              {content}
            </Link>
          ) : (
            <div
              key={task.id}
              role="treeitem"
              aria-level={depth + 1}
              aria-disabled="true"
              className={cn(className, "opacity-60")}
              style={style}
            >
              {content}
            </div>
          );
        })}
      </div>
    </section>
  );
}
