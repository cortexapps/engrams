/** The automation's agent on the canvas: one node under every way in. Each
 * way in's column ends on a stub that says what it does with the agent
 * (starts it, reaches it, closes it, or nothing); a bus joins the stubs and
 * drops to the node. The node is the same card as a step; in replay it
 * carries the agent step's status and sentence.
 *
 * No agent yet: a dashed "Add an agent" node in its place (never on a
 * built-in, whose shape is fixed). */

import { Bot, Plus } from "lucide-react";
import type { KeyboardEvent } from "react";

import { StatusDot, type StatusTone } from "@/components/status-dot";
import type { EntrypointRole } from "@/lib/automation-blocks";
import { cn } from "@/lib/utils";

import type { CanvasNodeStatus } from "./CanvasNode";
import { NODE_H, NODE_W } from "./layout";

const STATUS_TONE: Record<CanvasNodeStatus, StatusTone> = {
  running: "active",
  ok: "nominal",
  failed: "critical",
  skipped: "muted",
};

/** The stub's caption: what the way in does with the agent. */
export const ROLE_CAPTION: Record<EntrypointRole, string> = {
  start: "starts it",
  reach: "reaches it",
  close: "closes it",
  none: "no agent",
};

/** The connector under one way in's column. */
export function AgentStub({ role, dimmed }: { role: EntrypointRole; dimmed?: boolean }) {
  return (
    <div
      className={cn("flex shrink-0 flex-col items-center", dimmed && "opacity-40")}
      data-testid="agent-stub"
      data-role={role}
    >
      <div
        className={cn(
          "h-5 w-px",
          role === "none" ? "border-l border-dashed border-foreground/30" : "bg-foreground/45",
        )}
      />
      <span className="rounded-sm bg-background px-1.5 text-2xs font-semibold leading-4 text-muted-foreground">
        {ROLE_CAPTION[role]}
      </span>
    </div>
  );
}

export interface AgentNodeProps {
  /** The second line: the profile's name and the scope. */
  summary: string;
  selected: boolean;
  errored: boolean;
  status?: CanvasNodeStatus;
  /** Replay: the run never reached the agent. */
  dimmed?: boolean;
  onSelect: () => void;
}

export function AgentNode({
  summary,
  selected,
  errored,
  status,
  dimmed,
  onSelect,
}: AgentNodeProps) {
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onSelect();
    }
  };
  return (
    <div
      role="button"
      tabIndex={0}
      data-testid="agent-node"
      data-dimmed={dimmed ? "true" : undefined}
      aria-current={selected ? "true" : undefined}
      onClick={onSelect}
      onKeyDown={onKeyDown}
      style={{
        width: NODE_W,
        height: NODE_H,
        ...(errored
          ? { borderColor: "color-mix(in oklch, var(--color-destructive) 60%, transparent)" }
          : {}),
      }}
      className={cn(
        "relative flex flex-col justify-center gap-0.5 rounded-lg border bg-card px-3 py-2.5 text-left shadow-[0_1px_2px_oklch(0_0_0/.06)] transition-[border-color,outline-color] duration-150 outline-none",
        selected ? "outline-2 outline-offset-1 outline-ring" : "hover:border-ring/40",
        dimmed && "opacity-40",
      )}
    >
      {status && (
        <span className="absolute -top-1 -right-1" aria-label={`step ${status}`} role="img">
          <StatusDot tone={STATUS_TONE[status]} size={10} />
        </span>
      )}
      <div className="flex items-center gap-2">
        <Bot className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />
        <span className="min-w-0 flex-1 truncate text-sm font-semibold">Agent</span>
        {errored && <StatusDot tone="critical" size={6} label="has errors" />}
        <span className="max-w-[45%] shrink-0 truncate font-mono text-2xs text-muted-foreground">
          agent
        </span>
      </div>
      <div className="truncate pl-[22px] text-xs text-muted-foreground">{summary}</div>
    </div>
  );
}

export function AddAgentNode({ onAdd }: { onAdd: () => void }) {
  return (
    <button
      type="button"
      data-testid="add-agent"
      onClick={onAdd}
      style={{ width: NODE_W }}
      className="flex h-9 items-center justify-center gap-1.5 rounded-lg border border-dashed bg-transparent text-xs text-muted-foreground transition-colors hover:border-ring/50 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/60 focus-visible:outline-none"
    >
      <Plus className="size-3.5" aria-hidden />
      Add an agent
    </button>
  );
}

/** The bus: a line across the way-in columns' centres, a drop to the node.
 * `columns` is how many ways in sit above (each the same width), so the
 * line runs from the first column's centre to the last's. */
export function AgentBus({ columns, children }: { columns: number; children: React.ReactNode }) {
  const inset = `calc(100% / ${columns * 2})`;
  return (
    <div className="flex shrink-0 flex-col items-center" data-testid="agent-bus">
      <div className="relative h-px w-full">
        {columns > 1 && (
          <div
            className="absolute inset-y-0 border-t border-foreground/45"
            style={{ left: inset, right: inset }}
          />
        )}
      </div>
      <div className="h-5 w-px bg-foreground/45" />
      {children}
    </div>
  );
}
