/** One node card on the canvas: block variant or trigger variant.
 *
 * Keeps the BlockList row's external contract byte-for-byte —
 * `data-testid="block-row-<id>"`, `aria-current`, the "has errors" dot, the
 * `Remove <id>` label — so the editor-level tests pass unchanged. Ghost and
 * status are pure class/element toggles with no layout impact: the AI
 * drafting and run-replay features wire them later without touching
 * geometry.
 */

import { ArrowDown, ArrowUp, MoreVertical, Trash2, Zap } from "lucide-react";
import type { KeyboardEvent, ReactNode } from "react";

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

const STATUS_CLASS: Record<CanvasNodeStatus, string> = {
  running: "bg-instrument-caution",
  ok: "bg-instrument-nominal",
  failed: "bg-instrument-critical",
  skipped: "bg-muted-foreground/50",
};

export interface CanvasNodeProps {
  node: LayoutNode;
  /** Absent = the trigger variant. */
  block?: BlockDef;
  triggerSummary?: string;
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
  const label = spec?.label ?? "Trigger";
  const summary = block ? spec!.summary(block.config) : (triggerSummary ?? "");

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
      <span
        className={cn("absolute -top-1.5 -right-1.5 size-3 rounded-full", STATUS_CLASS[status])}
        aria-label={`step ${status}`}
      />
    );
  }

  return (
    <div
      ref={nodeRef}
      role="button"
      tabIndex={0}
      data-testid={block ? `block-row-${block.id}` : "block-row-trigger"}
      aria-current={selected ? "true" : undefined}
      onClick={onSelect}
      onKeyDown={onKeyDown}
      style={{ left: node.x, top: node.y, width: node.w, height: node.h }}
      className={cn(
        "group absolute z-10 flex items-center gap-2 rounded-lg border px-3 py-2 text-sm transition-[border-color,box-shadow]",
        ghost ? "border-dashed bg-transparent opacity-60" : "bg-card shadow-xs",
        selected ? "ring-ring ring-2" : "hover:border-ring/40 hover:shadow-sm",
        errored ? "border-destructive/60" : block ? "border-border" : "border-primary/50",
      )}
    >
      {statusChip}
      <Icon
        className={cn(
          "size-5 shrink-0 rounded p-0.5",
          block ? "bg-muted text-muted-foreground" : "bg-primary text-primary-foreground",
        )}
        aria-hidden
      />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate font-medium">{label}</span>
          {block && <code className="text-muted-foreground truncate text-xs">{block.id}</code>}
          {errored && (
            <span
              className="bg-destructive size-1.5 shrink-0 rounded-full"
              aria-label="has errors"
            />
          )}
        </div>
        <div className="text-muted-foreground truncate text-xs">{summary}</div>
      </div>
      {block && !locked && (
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              type="button"
              variant="ghost"
              size="icon"
              className="size-7 opacity-0 group-hover:opacity-100 focus-visible:opacity-100 data-[state=open]:opacity-100"
              aria-label={`Actions for ${block.id}`}
              onClick={(e) => e.stopPropagation()}
            >
              <MoreVertical className="size-4" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end" onClick={(e) => e.stopPropagation()}>
            <DropdownMenuItem disabled={!canMoveUp} onSelect={onMoveUp}>
              <ArrowUp className="size-4" aria-hidden /> Move up
            </DropdownMenuItem>
            <DropdownMenuItem disabled={!canMoveDown} onSelect={onMoveDown}>
              <ArrowDown className="size-4" aria-hidden /> Move down
            </DropdownMenuItem>
            <DropdownMenuItem
              variant="destructive"
              aria-label={`Remove ${block.id}`}
              onSelect={onRemove}
            >
              <Trash2 className="size-4" aria-hidden /> Remove
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      )}
    </div>
  );
}
