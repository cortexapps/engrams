/** One way in, drawn: the trigger node at the top, the steps below it as
 * cards, branch legs as dashed lanes, and a dashed "Add step" node at the
 * tail. Layout is `layoutCanvas`'s pure geometry; everything render-only
 * (selection, lock, errors, ghosts, run status) resolves by node id here.
 *
 * The canvas scales to fit its column unless the Build tab hands it a zoom. */

import { Lock, Plus } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  insertableBlockKinds,
  type BlockDef,
  type ListPath,
  type TriggerSpec,
} from "@/lib/automation-blocks";

import { CanvasNode, type CanvasNodeStatus } from "./CanvasNode";
import { EdgeLayer } from "./EdgeLayer";
import { InsertMenu } from "./InsertMenu";
import { layoutCanvas, MAX_SCALE, MIN_SCALE, NODE_W, TRIGGER_ROW_ID } from "./layout";

export { TRIGGER_ROW_ID };
export type { CanvasNodeStatus };

export interface CanvasProps {
  trigger: TriggerSpec;
  triggerSummary: string;
  /** The way in this canvas draws (its trigger node's second line and the
   * canvas's test id). Omitted for a single-entrypoint automation. */
  entrypointId?: string;
  blocks: readonly BlockDef[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  /** Ids of blocks carrying a server error (node error dot). */
  erroredIds: ReadonlySet<string>;
  /** Built-in: locked structure. */
  locked: boolean;
  onInsert: (at: ListPath, index: number, kind: string) => void;
  onMove: (at: ListPath, from: number, to: number) => void;
  onRemove: (id: string) => void;
  /** AI drafting (phase 3): render these blocks as ghosts. */
  ghostIds?: ReadonlySet<string>;
  /** Run replay (later): per-block status chips. */
  nodeStatus?: Readonly<Record<string, CanvasNodeStatus>>;
  /** "fit" (default) scales down to the column; a number is an explicit zoom. */
  zoom?: number | "fit";
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

export function Canvas({
  trigger,
  triggerSummary,
  entrypointId,
  blocks,
  selectedId,
  onSelect,
  erroredIds,
  locked,
  onInsert,
  onMove,
  onRemove,
  ghostIds,
  nodeStatus,
  zoom = "fit",
}: CanvasProps) {
  void trigger; // the trigger node's title is `triggerSummary`
  const layout = useMemo(() => layoutCanvas(blocks), [blocks]);
  const blocksById = useMemo(() => {
    const map = new Map<string, BlockDef>();
    const walk = (list: readonly BlockDef[]) => {
      for (const block of list) {
        map.set(block.id, block);
        if (block.then) walk(block.then);
        if (block.else) walk(block.else);
        if (block.body) walk(block.body);
      }
    };
    walk(blocks);
    return map;
  }, [blocks]);

  // Scale-to-fit: the canvas shrinks (never grows) to the column width.
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const [containerWidth, setContainerWidth] = useState(0);
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const update = () => setContainerWidth(el.clientWidth);
    update();
    const observer = new ResizeObserver(update);
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  // jsdom (stubbed observer) lands on width 0 → scale 1, untransformed.
  const fit = containerWidth > 0 ? clamp(containerWidth / layout.width, MIN_SCALE, MAX_SCALE) : 1;
  const scale = zoom === "fit" ? fit : clamp(zoom, MIN_SCALE, MAX_SCALE);

  // Roving arrow-key focus in document order (nodes render in that order).
  const nodeRefs = useRef(new Map<string, HTMLDivElement>());
  const focusStep = (fromId: string, delta: 1 | -1) => {
    const order = layout.nodes.map((n) => n.id);
    const next = order[order.indexOf(fromId) + delta];
    if (next) nodeRefs.current.get(next)?.focus();
  };

  const tail = layout.edges.find((edge) => edge.kind === "tail");

  return (
    <div
      className="flex h-full min-w-0 flex-col"
      data-testid={entrypointId ? `entrypoint-canvas-${entrypointId}` : "block-canvas"}
    >
      <div
        ref={scrollRef}
        className="min-h-0 flex-1 overflow-auto"
        aria-label={entrypointId ? `Steps for ${entrypointId}` : "Automation blocks"}
      >
        <div style={{ width: layout.width * scale, height: layout.height * scale }}>
          <div
            className="relative"
            style={{
              width: layout.width,
              height: layout.height,
              transform: `scale(${scale})`,
              transformOrigin: "top left",
            }}
          >
            <EdgeLayer layout={layout} />
            {layout.nodes.map((node) => {
              const block = node.id === TRIGGER_ROW_ID ? undefined : blocksById.get(node.id);
              return (
                <CanvasNode
                  key={node.id}
                  node={node}
                  block={block}
                  triggerSummary={triggerSummary}
                  triggerDetail={entrypointId}
                  selected={selectedId === (block ? node.id : TRIGGER_ROW_ID)}
                  errored={erroredIds.has(block ? node.id : TRIGGER_ROW_ID)}
                  locked={locked}
                  ghost={block ? ghostIds?.has(node.id) : false}
                  status={block ? nodeStatus?.[node.id] : undefined}
                  canMoveUp={node.index !== undefined && node.index > 0}
                  canMoveDown={
                    node.index !== undefined &&
                    node.listLength !== undefined &&
                    node.index < node.listLength - 1
                  }
                  onSelect={() => onSelect(block ? node.id : TRIGGER_ROW_ID)}
                  onMoveUp={() => {
                    if (node.at && node.index !== undefined) {
                      onMove(node.at, node.index, node.index - 1);
                    }
                  }}
                  onMoveDown={() => {
                    if (node.at && node.index !== undefined) {
                      onMove(node.at, node.index, node.index + 1);
                    }
                  }}
                  onRemove={() => onRemove(node.id)}
                  onFocusStep={(delta) => focusStep(node.id, delta)}
                  nodeRef={(el) => {
                    if (el) nodeRefs.current.set(node.id, el);
                    else nodeRefs.current.delete(node.id);
                  }}
                />
              );
            })}
            {!locked &&
              layout.edges
                .filter((edge) => edge.insert && edge.kind !== "tail")
                .map((edge) => (
                  <InsertMenu
                    key={`insert-${edge.id}`}
                    x={edge.insert!.x}
                    y={edge.insert!.y}
                    at={edge.insert!.at}
                    index={edge.insert!.index}
                    onInsert={onInsert}
                  />
                ))}
            {!locked && tail?.insert && (
              <AddStepNode
                x={tail.points.at(-1)!.x}
                y={tail.points.at(-1)!.y}
                at={tail.insert.at}
                index={tail.insert.index}
                onInsert={onInsert}
              />
            )}
          </div>
        </div>
      </div>
      {locked && (
        <div className="flex shrink-0 items-center gap-1 px-3 py-1.5 text-2xs text-muted-foreground">
          <Lock className="size-3" aria-hidden /> structure set by the built-in
        </div>
      )}
    </div>
  );
}

/** The dashed node at the end of the trail: the one place to append a step
 * when the "+" affordances between steps are not what you reach for. Carries
 * the same accessible name as those affordances — it IS one. */
function AddStepNode({
  x,
  y,
  at,
  index,
  onInsert,
}: {
  x: number;
  y: number;
  at: ListPath;
  index: number;
  onInsert: (at: ListPath, index: number, kind: string) => void;
}) {
  return (
    <div
      className="absolute z-20 -translate-x-1/2 -translate-y-1/2"
      style={{ left: x, top: y, width: NODE_W }}
    >
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <button
            type="button"
            aria-label="Insert block here"
            data-testid="add-step"
            className="flex h-9 w-full items-center justify-center gap-1.5 rounded-lg border border-dashed bg-transparent text-xs text-muted-foreground transition-colors hover:border-ring/50 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/60 focus-visible:outline-none"
          >
            <Plus className="size-3.5" aria-hidden />
            Add step
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="center">
          {insertableBlockKinds().map((spec) => (
            <DropdownMenuItem key={spec.kind} onSelect={() => onInsert(at, index, spec.kind)}>
              <spec.icon className="size-4" aria-hidden />
              {spec.label}
            </DropdownMenuItem>
          ))}
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}
