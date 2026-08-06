import { useMemo, useState } from "react";
import { ChevronUpIcon, CircleCheckIcon, CircleDashedIcon, CircleDotIcon } from "lucide-react";
import { Markdown } from "@/components/Markdown";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";
import type { IndexedEvent } from "../events";
import {
  extractAgentTasks,
  type AgentTask,
  type AgentTaskStatus,
} from "./session-thread/agentTasks";
import { extractPlanRevisions } from "./session-thread/planRevisions";

const STATUS_ORDER: Record<AgentTaskStatus, number> = {
  in_progress: 0,
  pending: 1,
  completed: 2,
};

function StatusIcon({ status }: { status: AgentTaskStatus }) {
  switch (status) {
    case "in_progress":
      // `ring` is the app's established "this is running" tone (see Glyph's
      // toneFor) — racing green on paper, lime on the dark ground. `primary`
      // would have said "this is a thing you can click".
      return <CircleDotIcon aria-label="In progress" className="size-4 shrink-0 text-ring" />;
    case "completed":
      return (
        <CircleCheckIcon
          aria-label="Completed"
          className="size-4 shrink-0 text-instrument-nominal-ink"
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
    <li className="flex items-start gap-2.5 px-4 py-2" data-testid="work-dock-task-row">
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

export function WorkDock({ events }: { events: IndexedEvent[] }) {
  const tasks = useMemo(() => extractAgentTasks(events), [events]);
  const revisions = useMemo(() => extractPlanRevisions(events), [events]);
  const orderedTasks = useMemo(
    () => [...tasks].sort((a, b) => STATUS_ORDER[a.status] - STATUS_ORDER[b.status]),
    [tasks],
  );
  const [expanded, setExpanded] = useState(false);
  const [tab, setTab] = useState<"tasks" | "plan">(tasks.length > 0 ? "tasks" : "plan");
  const [selectedRevision, setSelectedRevision] = useState<number | null>(null);

  if (tasks.length === 0 && revisions.length === 0) return null;

  const hasTasks = tasks.length > 0;
  const hasPlan = revisions.length > 0;
  const activeTab = hasTasks && hasPlan ? tab : hasTasks ? "tasks" : "plan";
  const done = tasks.filter((task) => task.status === "completed").length;
  const inProgress = tasks.find((task) => task.status === "in_progress");
  const allTasksCompleted = hasTasks && done === tasks.length;
  const progressLabel = hasTasks ? `${done} of ${tasks.length}` : `rev ${revisions.length}`;
  const currentRevision = selectedRevision ?? revisions.length - 1;
  const revision = revisions[currentRevision];

  const body =
    activeTab === "tasks" ? (
      <ul className="divide-y divide-border/60">
        {orderedTasks.map((task) => (
          <TaskRow key={task.createdBy} task={task} />
        ))}
      </ul>
    ) : revision ? (
      <div>
        {revisions.length > 1 && (
          <div className="flex flex-wrap items-center gap-1.5 border-b px-4 py-2">
            {revisions.map((candidate, index) => (
              <button
                key={candidate.toolCallId}
                type="button"
                onClick={() => setSelectedRevision(index === revisions.length - 1 ? null : index)}
                className={cn(
                  "h-6 rounded-md px-2 text-xs font-medium",
                  index === currentRevision
                    ? "bg-accent text-foreground"
                    : "text-muted-foreground hover:bg-accent/60",
                )}
              >
                rev {index + 1}
              </button>
            ))}
          </div>
        )}
        <div className="mx-auto max-w-[70ch] px-6 py-4 text-sm leading-relaxed">
          <Markdown text={revision.plan} />
        </div>
      </div>
    ) : null;

  return (
    <div className="flex shrink-0 flex-col bg-background">
      {expanded && (
        <div className="flex max-h-[40vh] min-h-0 flex-col overflow-hidden">
          {hasTasks && hasPlan ? (
            <Tabs
              value={activeTab}
              onValueChange={(value) => setTab(value as "tasks" | "plan")}
              className="min-h-0 flex-1 gap-0 overflow-hidden"
            >
              <div className="shrink-0 px-3 py-2">
                <TabsList className="h-7">
                  <TabsTrigger value="tasks" className="px-3 py-0 text-xs">
                    Tasks
                  </TabsTrigger>
                  <TabsTrigger value="plan" className="px-3 py-0 text-xs">
                    Plan
                  </TabsTrigger>
                </TabsList>
              </div>
              <TabsContent value={activeTab} className="min-h-0 flex-1 overflow-auto">
                {body}
              </TabsContent>
            </Tabs>
          ) : (
            <div className="min-h-0 flex-1 overflow-auto">{body}</div>
          )}
        </div>
      )}
      <button
        type="button"
        aria-expanded={expanded}
        onClick={() => setExpanded((open) => !open)}
        className="flex h-10 w-full shrink-0 items-center gap-2 border-t px-4 text-left text-sm hover:bg-accent/50"
      >
        {inProgress ? (
          <CircleDotIcon className="size-4 shrink-0 animate-pulse text-ring" />
        ) : allTasksCompleted ? (
          <CircleCheckIcon className="size-4 shrink-0 text-instrument-nominal-ink" />
        ) : (
          <CircleDashedIcon className="size-4 shrink-0 text-muted-foreground" />
        )}
        <span className="shrink-0 tabular-nums">{progressLabel}</span>
        {inProgress && <span className="min-w-0 flex-1 truncate">{inProgress.subject}</span>}
        {!inProgress && <span className="min-w-0 flex-1" />}
        <ChevronUpIcon
          className={cn(
            "size-4 shrink-0 text-muted-foreground transition-transform",
            expanded && "rotate-180",
          )}
        />
      </button>
    </div>
  );
}
