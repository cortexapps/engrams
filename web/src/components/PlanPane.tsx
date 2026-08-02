import { useMemo, useState } from "react";
import { Markdown } from "@/components/Markdown";
import { cn } from "@/lib/utils";
import type { IndexedEvent } from "../events";

// ADR 0107: the read-only plan reading surface. The thread's PlanCard is the
// DECISION surface (approve/reject lives there and only there — a background
// pane must never hold a stale approve button); this pane is for READING
// 13KB documents comfortably, with a revision switcher across
// reject → revise cycles.

interface PlanRevision {
  toolCallId: string;
  plan: string;
  at: string;
}

/** Every plan proposal in event order (revision 1..N). */
export function extractPlanRevisions(events: IndexedEvent[]): PlanRevision[] {
  const revisions: PlanRevision[] = [];
  for (const { event } of events) {
    if (event.type !== "tool_call_requested" || event.name !== "exit_plan_mode") continue;
    try {
      const raw: unknown = JSON.parse(event.args_json);
      const plan = (raw as Record<string, unknown> | null)?.plan;
      if (typeof plan === "string" && plan.length > 0) {
        revisions.push({ toolCallId: event.tool_call_id, plan, at: event.at });
      }
    } catch {
      // Malformed args never break the pane.
    }
  }
  return revisions;
}

export function PlanPane({ events }: { events: IndexedEvent[] }) {
  const revisions = useMemo(() => extractPlanRevisions(events), [events]);
  // null = follow the latest revision as new ones arrive.
  const [selected, setSelected] = useState<number | null>(null);
  const current = selected ?? revisions.length - 1;
  const revision = revisions[current];

  if (!revision) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-muted-foreground italic">
        No plan proposed yet.
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      {revisions.length > 1 && (
        <div className="flex shrink-0 flex-wrap items-center gap-1.5 border-b px-4 py-2">
          {revisions.map((r, i) => (
            <button
              key={r.toolCallId}
              type="button"
              onClick={() => setSelected(i === revisions.length - 1 ? null : i)}
              className={cn(
                "h-6 rounded-md px-2 text-xs font-medium",
                i === current
                  ? "bg-accent text-foreground"
                  : "text-muted-foreground hover:bg-accent/60",
              )}
            >
              rev {i + 1}
            </button>
          ))}
        </div>
      )}
      <div className="min-h-0 flex-1 overflow-auto px-6 py-4">
        <div className="mx-auto max-w-[70ch] text-sm leading-relaxed">
          <Markdown text={revision.plan} />
        </div>
      </div>
    </div>
  );
}
