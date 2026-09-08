import { useState } from "react";

import type { AutomationDropBrief } from "@/gen/engram/app/v1/automation_pb";
import { relativeAge } from "@/lib/relative-time";

export type WorkstreamDrop = AutomationDropBrief & { automationId?: string };

export function WorkstreamDrops({ drops, now }: { drops: readonly WorkstreamDrop[]; now: number }) {
  const [expanded, setExpanded] = useState(false);
  const recent = drops.filter((drop) => {
    const droppedAt = Date.parse(drop.droppedAt);
    return !Number.isNaN(droppedAt) && now - droppedAt <= 24 * 60 * 60 * 1_000;
  });
  if (recent.length === 0) return null;

  const closed = recent.filter((drop) => drop.reason === "closed_instance").length;
  const unmatched = recent.filter(
    (drop) => drop.reason === "no_open_instance" || drop.reason === "no_handle_match",
  ).length;

  return (
    <div className="rounded-lg border border-dashed px-4 py-3 text-xs text-muted-foreground">
      <div
        className="flex flex-wrap items-center gap-x-1.5 gap-y-1"
        data-testid="recent-drops-summary"
      >
        <span>
          <span className="font-mono tabular-nums">{recent.length}</span> events did not fire in the
          last day — <span className="font-mono tabular-nums">{closed}</span> arrived for closed
          workstreams, <span className="font-mono tabular-nums">{unmatched}</span> matched no open
          workstream
        </span>
        <button
          type="button"
          className="font-medium text-foreground underline-offset-4 hover:underline"
          onClick={() => setExpanded((value) => !value)}
          aria-expanded={expanded}
        >
          {expanded ? "Hide them ↑" : "Show them →"}
        </button>
      </div>
      {expanded && (
        <ul className="mt-3 flex flex-col divide-y" data-testid="recent-drops">
          {recent.map((drop, index) => (
            <li
              key={`${drop.automationId ?? "automation"}-${drop.droppedAt}-${index}`}
              className="grid grid-cols-[minmax(0,1fr)_auto] gap-3 py-2 first:pt-0 last:pb-0"
            >
              <span className="min-w-0 truncate">
                <span className="font-mono">{drop.eventKey || "unknown event"}</span>
                {drop.detail && ` · ${drop.detail}`}
              </span>
              <span className="font-mono tabular-nums">{relativeAge(drop.droppedAt, now)}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
