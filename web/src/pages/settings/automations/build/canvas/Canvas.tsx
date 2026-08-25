/** The Builder canvas (replaces BlockList): the block tree rendered as
 * nodes + orthogonal edges, scaled to fit its panel. Same callback
 * contract as the list it replaced; layout is `layoutCanvas`'s pure
 * geometry, and everything render-only (selection, lock, errors, ghosts,
 * run status) resolves by node id here. */

import { Lock } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import type { BlockDef, ListPath, TriggerSpec } from "@/lib/automation-blocks";

import { CanvasNode, type CanvasNodeStatus } from "./CanvasNode";
import { EdgeLayer } from "./EdgeLayer";
import { InsertMenu } from "./InsertMenu";
import { layoutCanvas, MAX_SCALE, MIN_SCALE, TRIGGER_ROW_ID } from "./layout";

export { TRIGGER_ROW_ID };
export type { CanvasNodeStatus };

export interface CanvasProps {
  trigger: TriggerSpec;
  triggerSummary: string;
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
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

export function Canvas({
  trigger,
  triggerSummary,
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
}: CanvasProps) {
  void trigger; // the trigger node's summary line is `triggerSummary`
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

  // Scale-to-fit: the canvas shrinks (never grows) to the panel width.
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
  const scale = containerWidth > 0 ? clamp(containerWidth / layout.width, MIN_SCALE, MAX_SCALE) : 1;

  // Roving arrow-key focus in document order (nodes render in that order).
  const nodeRefs = useRef(new Map<string, HTMLDivElement>());
  const focusStep = (fromId: string, delta: 1 | -1) => {
    const order = layout.nodes.map((n) => n.id);
    const next = order[order.indexOf(fromId) + delta];
    if (next) nodeRefs.current.get(next)?.focus();
  };

  return (
    <div className="flex h-full flex-col" data-testid="block-canvas">
      <div className="text-muted-foreground flex items-center justify-between px-3 py-2 text-xs font-medium">
        <span>Blocks</span>
        {locked && (
          <span className="inline-flex items-center gap-1">
            <Lock className="size-3" aria-hidden /> structure set by the built-in
          </span>
        )}
      </div>
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto" aria-label="Automation blocks">
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
                .filter((edge) => edge.insert)
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
          </div>
        </div>
      </div>
    </div>
  );
}
