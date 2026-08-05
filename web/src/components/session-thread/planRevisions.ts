import type { IndexedEvent } from "../../events";

export interface PlanRevision {
  toolCallId: string;
  plan: string;
  at: string;
}

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
      // Malformed arguments do not break the pane.
    }
  }
  return revisions;
}
