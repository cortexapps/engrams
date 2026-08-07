import type { ToolCallMessagePartProps } from "@assistant-ui/react";
import {
  CircleCheckIcon,
  CircleDashedIcon,
  CircleDotIcon,
  CircleMinusIcon,
  ListTodoIcon,
  Loader2Icon,
  XCircleIcon,
} from "lucide-react";
import { cn } from "@/lib/utils";
import type { TaskCallDisplay } from "./agentTasks";

// Compact checklist row for the agent's TaskCreate/TaskUpdate bookkeeping —
// the Claude-Code-style one-liner ("◌ subject · task added", "✓ subject ·
// completed") the TASK_TOOL synthetic part renders instead of the generic
// JSON card. The side panel's Tasks tab shows the full live list; this row
// only narrates one transition in place.

function meaning(args: TaskCallDisplay | undefined): {
  Icon: typeof ListTodoIcon;
  iconClass: string;
  verb: string;
  done: boolean;
} {
  if (args?.action === "create") {
    return {
      Icon: CircleDashedIcon,
      iconClass: "text-muted-foreground",
      verb: "task added",
      done: false,
    };
  }
  switch (args?.status) {
    case "in_progress":
      // `ring` is the running tone across the app (Glyph's toneFor); lime is
      // reserved for things you can act on.
      return { Icon: CircleDotIcon, iconClass: "text-ring", verb: "started", done: false };
    case "completed":
      return {
        Icon: CircleCheckIcon,
        iconClass: "text-instrument-nominal-ink",
        verb: "completed",
        done: true,
      };
    case "deleted":
      return {
        Icon: CircleMinusIcon,
        iconClass: "text-muted-foreground",
        verb: "removed",
        done: false,
      };
    case "pending":
      return {
        Icon: CircleDashedIcon,
        iconClass: "text-muted-foreground",
        verb: "back to pending",
        done: false,
      };
    default:
      // A subject/description/dependency edit with no status change.
      return {
        Icon: ListTodoIcon,
        iconClass: "text-muted-foreground",
        verb: "updated",
        done: false,
      };
  }
}

export function TaskToolPart({ args, status }: ToolCallMessagePartProps<TaskCallDisplay, string>) {
  const running = status?.type === "running";
  const failed = status?.type === "incomplete";
  const { Icon, iconClass, verb, done } = meaning(args);

  return (
    <div
      className="flex min-w-0 items-center gap-2 rounded-lg border bg-card/40 px-3 py-2 text-sm"
      data-testid="task-tool-part"
    >
      <Icon className={cn("size-3.5 shrink-0", iconClass)} />
      <span
        className={cn(
          "min-w-0 flex-1 truncate",
          done ? "text-muted-foreground line-through" : "text-foreground",
        )}
      >
        {args?.subject || "Task"}
      </span>
      <span className="shrink-0 text-xs text-muted-foreground">{verb}</span>
      {running && (
        <Loader2Icon
          aria-label="Updating task list"
          className="size-3.5 shrink-0 animate-spin text-muted-foreground"
        />
      )}
      {failed && (
        <XCircleIcon
          aria-label="Task update failed"
          className="size-3.5 shrink-0 text-destructive"
        />
      )}
    </div>
  );
}
