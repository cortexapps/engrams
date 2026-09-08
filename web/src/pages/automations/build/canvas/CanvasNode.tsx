/** One node card on the canvas: a step (card on the sheet) or a trigger (the
 * way in, on the cover's fill so it reads as a header, not as another step).
 *
 * Keeps the external contract — `data-testid="block-row-<id>"`,
 * `aria-current`, the "has errors" dot, the `Remove <id>` label — so the
 * editor-level tests pass unchanged. Ghost and status are pure class/element
 * toggles with no layout impact. */

import { ArrowDown, ArrowUp, MoreVertical, Trash2, Zap } from "lucide-react";
import type { KeyboardEvent, ReactNode } from "react";

import { StatusDot, type StatusTone } from "@/components/status-dot";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { blockKind, type BlockDef } from "@/lib/automation-blocks";
import { cn } from "@/lib/utils";

import type { LayoutNode } from "./layout";

export type CanvasNodeStatus = "running" | "ok" | "failed" | "skipped";

const STATUS_TONE: Record<CanvasNodeStatus, StatusTone> = {
  running: "active",
  ok: "nominal",
  failed: "critical",
  skipped: "muted",
};

export interface CanvasNodeProps {
  node: LayoutNode;
  /** Absent = the trigger variant. */
  block?: BlockDef;
  /** The trigger variant's title: the human trigger ("Every day at 02:00 UTC"). */
  triggerSummary?: string;
  /** The trigger variant's second line, e.g. the way in it names. */
  triggerDetail?: string;
  selected: boolean;
  errored: boolean;
  locked: boolean;
  ghost?: boolean;
  status?: CanvasNodeStatus;
  canMoveUp: boolean;
  canMoveDown: boolean;
  onSelect: () => void;
  onMoveUp: () => void;
  onMoveDown: () => void;
  onRemove: () => void;
  /** Arrow-key roving focus, owned by Canvas. */
  onFocusStep: (delta: 1 | -1) => void;
  nodeRef: (el: HTMLDivElement | null) => void;
}

export function CanvasNode({
  node,
  block,
  triggerSummary,
  triggerDetail,
  selected,
  errored,
  locked,
  ghost,
  status,
  canMoveUp,
  canMoveDown,
  onSelect,
  onMoveUp,
  onMoveDown,
  onRemove,
  onFocusStep,
  nodeRef,
}: CanvasNodeProps) {
  const spec = block ? blockKind(block.type) : null;
  const Icon = spec?.icon ?? Zap;
  const summary = block ? spec!.summary(block.config) : "";

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onSelect();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      onFocusStep(1);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      onFocusStep(-1);
    }
  };

  let statusChip: ReactNode = null;
  if (status) {
    statusChip = (
      <span className="absolute -top-1 -right-1" aria-label={`step ${status}`} role="img">
        <StatusDot tone={STATUS_TONE[status]} size={10} />
      </span>
    );
  }

  const trigger = !block;
  return (
    <div
      ref={nodeRef}
      role="button"
      tabIndex={0}
      data-testid={block ? `block-row-${block.id}` : "block-row-trigger"}
      aria-current={selected ? "true" : undefined}
      onClick={onSelect}
      onKeyDown={onKeyDown}
      style={{
        left: node.x,
        top: node.y,
        width: node.w,
        height: node.h,
        ...(errored
          ? { borderColor: "color-mix(in oklch, var(--color-destructive) 60%, transparent)" }
          : {}),
      }}
      className={cn(
        "group absolute z-10 flex flex-col justify-center gap-0.5 rounded-lg border px-3 pt-[11px] pb-3 text-left transition-[border-color,outline-color] duration-150 outline-none",
        trigger
          ? "border-transparent bg-sidebar text-sidebar-foreground"
          : ghost
            ? "border-dashed bg-transparent opacity-60"
            : "bg-card shadow-[0_1px_2px_oklch(0_0_0/.06)]",
        selected && "outline-2 outline-offset-1 outline-ring",
        !selected && !trigger && "hover:border-ring/40",
      )}
    >
      {statusChip}
      <div className="flex items-center gap-2">
        <Icon
          className={cn(
            "size-3.5 shrink-0",
            trigger ? "text-sidebar-primary" : "text-muted-foreground",
          )}
          aria-hidden
        />
        <span className="min-w-0 flex-1 truncate text-sm font-semibold">
          {trigger ? triggerSummary || "Trigger" : spec!.label}
        </span>
        {errored && <StatusDot tone="critical" size={6} label="has errors" />}
        <span
          className={cn(
            "shrink-0 font-mono text-2xs",
            trigger ? "text-sidebar-foreground/70" : "text-muted-foreground",
          )}
        >
          {trigger ? "trigger" : block!.type}
        </span>
        {block && !locked && (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                type="button"
                variant="ghost"
                size="icon-xs"
                className="-mr-1 opacity-0 group-hover:opacity-100 focus-visible:opacity-100 data-[state=open]:opacity-100"
                aria-label={`Actions for ${block.id}`}
                onClick={(e) => e.stopPropagation()}
              >
                <MoreVertical />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" onClick={(e) => e.stopPropagation()}>
              <DropdownMenuItem disabled={!canMoveUp} onSelect={onMoveUp}>
                <ArrowUp aria-hidden /> Move up
              </DropdownMenuItem>
              <DropdownMenuItem disabled={!canMoveDown} onSelect={onMoveDown}>
                <ArrowDown aria-hidden /> Move down
              </DropdownMenuItem>
              <DropdownMenuItem
                variant="destructive"
                aria-label={`Remove ${block.id}`}
                onSelect={onRemove}
              >
                <Trash2 aria-hidden /> Remove
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        )}
      </div>
      <div
        className={cn(
          "truncate pl-[22px] text-xs",
          trigger ? "font-mono text-sidebar-foreground/70" : "text-muted-foreground",
        )}
      >
        {trigger ? triggerDetail : summary}
      </div>
    </div>
  );
}
