import { Link } from "@tanstack/react-router";
import { GitBranch } from "lucide-react";

import type { Task } from "../../gen/engram/app/v1/task_pb";

export function ChildTaskTree({ descendants }: { descendants: readonly Task[] }) {
  if (descendants.length === 0) return null;
  const rows = [...descendants].sort((left, right) =>
    (left.canonicalTaskName ?? "").localeCompare(right.canonicalTaskName ?? ""),
  );
  return (
    <details className="group border-b bg-muted/20 px-4 py-2 text-xs">
      <summary className="flex cursor-pointer list-none items-center gap-1.5 font-medium text-muted-foreground">
        <GitBranch className="size-3.5" />
        {rows.length} child {rows.length === 1 ? "task" : "tasks"}
      </summary>
      <div className="mt-2 flex flex-col gap-1" role="tree" aria-label="Child tasks">
        {rows.map((child) => {
          const sessionId = child.sessions[0]?.sessionId;
          const depth = Math.max(0, (child.canonicalTaskName?.split("/").length ?? 1) - 1);
          const content = (
            <>
              <span className="min-w-0 flex-1 truncate font-mono">
                {child.canonicalTaskName ?? child.title ?? child.id}
              </span>
              <span className="shrink-0 text-muted-foreground">{child.status}</span>
            </>
          );
          return sessionId ? (
            <Link
              key={child.id}
              to="/sessions/$id"
              params={{ id: sessionId }}
              role="treeitem"
              className="flex items-center gap-3 rounded px-2 py-1 hover:bg-muted"
              style={{ marginInlineStart: `${depth * 0.75}rem` }}
            >
              {content}
            </Link>
          ) : (
            <div
              key={child.id}
              role="treeitem"
              className="flex items-center gap-3 rounded px-2 py-1 opacity-60"
              style={{ marginInlineStart: `${depth * 0.75}rem` }}
            >
              {content}
            </div>
          );
        })}
      </div>
    </details>
  );
}
