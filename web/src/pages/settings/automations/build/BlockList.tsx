/** The vertical block list (ADR 0119 phase 3.3).
 *
 * Branch renders its then/else lists indented; loop renders its body. On a
 * user automation rows drag to reorder within their list and an insert
 * affordance sits between rows. On a built-in the structure is locked: no
 * add/remove/reorder, a lock glyph in the header — only the inspector's
 * tunable properties change. */

import { GripVertical, Lock, Plus, Trash2, Zap } from "lucide-react";
import { useState, type DragEvent } from "react";

import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  blockKind,
  insertableBlockKinds,
  type BlockDef,
  type ListPath,
  type TriggerSpec,
} from "@/lib/automation-blocks";
import { cn } from "@/lib/utils";

export const TRIGGER_ROW_ID = "__trigger__";

export interface BlockListProps {
  trigger: TriggerSpec;
  triggerSummary: string;
  blocks: readonly BlockDef[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  /** Ids of blocks carrying a server error (row error dot). */
  erroredIds: ReadonlySet<string>;
  /** Built-in: locked structure. */
  locked: boolean;
  onInsert: (at: ListPath, index: number, kind: string) => void;
  onMove: (at: ListPath, from: number, to: number) => void;
  onRemove: (id: string) => void;
}

interface RowProps {
  block: BlockDef;
  depth: number;
  selected: boolean;
  errored: boolean;
  locked: boolean;
  onSelect: () => void;
  onRemove: () => void;
  dragHandlers: {
    onDragStart: (e: DragEvent) => void;
    onDragOver: (e: DragEvent) => void;
    onDrop: (e: DragEvent) => void;
    onDragEnd: () => void;
  };
}

function BlockRow({
  block,
  depth,
  selected,
  errored,
  locked,
  onSelect,
  onRemove,
  dragHandlers,
}: RowProps) {
  const spec = blockKind(block.type);
  const Icon = spec.icon;
  return (
    <div
      role="button"
      tabIndex={0}
      data-testid={`block-row-${block.id}`}
      aria-current={selected ? "true" : undefined}
      draggable={!locked}
      onClick={onSelect}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onSelect();
        }
      }}
      {...(locked ? {} : dragHandlers)}
      className={cn(
        "group flex items-center gap-2 rounded-md border px-2 py-1.5 text-sm",
        selected ? "border-ring bg-accent" : "border-transparent hover:bg-accent/60",
        errored && "border-destructive/60",
      )}
      style={{ marginLeft: depth * 16 }}
    >
      {!locked && (
        <GripVertical
          className="text-muted-foreground size-4 shrink-0 cursor-grab opacity-0 group-hover:opacity-100"
          aria-hidden
        />
      )}
      <Icon className="text-muted-foreground size-4 shrink-0" aria-hidden />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate font-medium">{spec.label}</span>
          <code className="text-muted-foreground truncate text-xs">{block.id}</code>
          {errored && (
            <span className="bg-destructive size-1.5 rounded-full" aria-label="has errors" />
          )}
        </div>
        <div className="text-muted-foreground truncate text-xs">{spec.summary(block.config)}</div>
      </div>
      {!locked && (
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="opacity-0 group-hover:opacity-100"
          aria-label={`Remove ${block.id}`}
          onClick={(e) => {
            e.stopPropagation();
            onRemove();
          }}
        >
          <Trash2 className="size-4" />
        </Button>
      )}
    </div>
  );
}

function InsertBetween({ onInsert, depth }: { onInsert: (kind: string) => void; depth: number }) {
  return (
    <div className="group/insert flex h-3 items-center" style={{ marginLeft: depth * 16 }}>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <button
            type="button"
            aria-label="Insert block here"
            className="text-muted-foreground hover:text-foreground flex w-full items-center gap-1 opacity-0 group-hover/insert:opacity-100 focus:opacity-100"
          >
            <span className="bg-border h-px flex-1" />
            <Plus className="size-3.5" aria-hidden />
            <span className="bg-border h-px flex-1" />
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="center">
          {insertableBlockKinds().map((spec) => (
            <DropdownMenuItem key={spec.kind} onSelect={() => onInsert(spec.kind)}>
              <spec.icon className="size-4" aria-hidden />
              {spec.label}
            </DropdownMenuItem>
          ))}
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}

interface ListProps extends Omit<BlockListProps, "trigger" | "triggerSummary"> {
  at: ListPath;
  list: readonly BlockDef[];
  depth: number;
}

function List(props: ListProps) {
  const { at, list, depth, selectedId, onSelect, erroredIds, locked, onInsert, onMove, onRemove } =
    props;
  const [dragFrom, setDragFrom] = useState<number | null>(null);

  const handlers = (index: number): RowProps["dragHandlers"] => ({
    onDragStart: (e) => {
      setDragFrom(index);
      e.dataTransfer.effectAllowed = "move";
    },
    onDragOver: (e) => {
      if (dragFrom !== null) e.preventDefault();
    },
    onDrop: (e) => {
      e.preventDefault();
      if (dragFrom !== null && dragFrom !== index) onMove(at, dragFrom, index);
      setDragFrom(null);
    },
    onDragEnd: () => setDragFrom(null),
  });

  return (
    <div className="flex flex-col">
      {!locked && <InsertBetween depth={depth} onInsert={(kind) => onInsert(at, 0, kind)} />}
      {list.map((block, index) => {
        const spec = blockKind(block.type);
        return (
          <div key={block.id} className="flex flex-col">
            <BlockRow
              block={block}
              depth={depth}
              selected={selectedId === block.id}
              errored={erroredIds.has(block.id)}
              locked={locked}
              onSelect={() => onSelect(block.id)}
              onRemove={() => onRemove(block.id)}
              dragHandlers={handlers(index)}
            />
            {spec.nests === "branch" && (
              <>
                <div
                  className="text-muted-foreground mt-1 text-xs font-medium"
                  style={{ marginLeft: (depth + 1) * 16 }}
                >
                  then
                </div>
                <List
                  {...props}
                  at={{ parentId: block.id, slot: "then" }}
                  list={block.then ?? []}
                  depth={depth + 1}
                />
                <div
                  className="text-muted-foreground mt-1 text-xs font-medium"
                  style={{ marginLeft: (depth + 1) * 16 }}
                >
                  else
                </div>
                <List
                  {...props}
                  at={{ parentId: block.id, slot: "else" }}
                  list={block.else ?? []}
                  depth={depth + 1}
                />
              </>
            )}
            {spec.nests === "loop" && (
              <>
                <div
                  className="text-muted-foreground mt-1 text-xs font-medium"
                  style={{ marginLeft: (depth + 1) * 16 }}
                >
                  repeat
                </div>
                <List
                  {...props}
                  at={{ parentId: block.id, slot: "body" }}
                  list={block.body ?? []}
                  depth={depth + 1}
                />
              </>
            )}
            {!locked && (
              <InsertBetween depth={depth} onInsert={(kind) => onInsert(at, index + 1, kind)} />
            )}
          </div>
        );
      })}
      {list.length === 0 && locked && (
        <div
          className="text-muted-foreground px-2 py-1 text-xs italic"
          style={{ marginLeft: depth * 16 }}
        >
          empty
        </div>
      )}
    </div>
  );
}

export function BlockList(props: BlockListProps) {
  const { trigger, triggerSummary, selectedId, onSelect, erroredIds, locked } = props;
  const triggerSelected = selectedId === TRIGGER_ROW_ID;
  return (
    <div className="flex flex-col gap-1" data-testid="block-list">
      <div className="text-muted-foreground flex items-center justify-between px-2 text-xs font-medium">
        <span>Blocks</span>
        {locked && (
          <span className="inline-flex items-center gap-1">
            <Lock className="size-3" aria-hidden /> structure set by the built-in
          </span>
        )}
      </div>
      <div
        role="button"
        tabIndex={0}
        data-testid="block-row-trigger"
        aria-current={triggerSelected ? "true" : undefined}
        onClick={() => onSelect(TRIGGER_ROW_ID)}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onSelect(TRIGGER_ROW_ID);
          }
        }}
        className={cn(
          "flex items-center gap-2 rounded-md border px-2 py-1.5 text-sm",
          triggerSelected ? "border-ring bg-accent" : "border-transparent hover:bg-accent/60",
          erroredIds.has(TRIGGER_ROW_ID) && "border-destructive/60",
        )}
      >
        <Zap
          className="bg-primary text-primary-foreground size-5 shrink-0 rounded p-0.5"
          aria-hidden
        />
        <div className="min-w-0 flex-1">
          <div className="font-medium">Trigger</div>
          <div className="text-muted-foreground truncate text-xs">
            {triggerSummary || trigger.kind}
          </div>
        </div>
      </div>
      <List {...props} at={{ root: true }} list={props.blocks} depth={0} />
    </div>
  );
}
