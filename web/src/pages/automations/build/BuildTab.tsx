/** The Build tab: the canvas (one column per way in) with the inspector as a
 * side sheet on the right and the test drawer along the bottom.
 *
 * Owns nothing durable — the shell holds the draft definition and passes
 * change callbacks down; this component is layout + selection. Block ids are
 * unique automation-wide, so selecting a node in any column also selects that
 * column's way in for the inspector. */

import { ChevronDown, ChevronUp, MoreHorizontal, Plus, Trash2, X } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";

import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import type { AutomationTestState } from "@/hooks/useAutomationTest";
import {
  addEntrypoint,
  blockIdsOutsideEntrypoint,
  blockKind,
  entrypointIdError,
  entrypointIds,
  findBlock,
  insertBlock,
  MAIN_ENTRYPOINT_ID,
  mergeEntrypoint,
  moveBlock,
  nextBlockId,
  projectEntrypoint,
  removeBlock,
  removeEntrypoint,
  replaceBlock,
  walkBlocks,
  type AutomationDefinition,
  type BlockDef,
  type BlockErrorRef,
  type ListPath,
  type TriggerSpec,
} from "@/lib/automation-blocks";
import { cn } from "@/lib/utils";

import { BlockInspector } from "./BlockInspector";
import { Canvas, TRIGGER_ROW_ID } from "./canvas/Canvas";
import { MAX_SCALE, MIN_SCALE } from "./canvas/layout";
import { TestPanel } from "./test/TestPanel";
import { TriggerInspector } from "./TriggerInspector";

export interface BuildTabProps {
  /** The whole automation, every way in included. */
  definition: AutomationDefinition;
  /** The way in the inspector edits. */
  entrypointId: string;
  onSelectEntrypoint: (id: string) => void;
  onChange: (next: AutomationDefinition) => void;
  builtin: boolean;
  errors: readonly BlockErrorRef[];
  /** The human trigger for a way in, for its trigger node's title. */
  triggerSummaryFor: (entrypointId: string) => string;
  /** The test-with-sample state; drives the bottom drawer. */
  test?: AutomationTestState;
  /** Tests inject a stand-in for the drawer's body. */
  testPanel?: ReactNode;
  /** Live variable values keyed by path for the selected sample. */
  variableValues?: Readonly<Record<string, string>>;
}

/** Static variable paths available to a block: trigger/inputs/event roots
 * plus steps.<id>.<output> for every block above it in document order. */
function variablePathsFor(definition: AutomationDefinition, selectedId: string | null): string[] {
  const paths = ["trigger.kind", "trigger.event", "trigger.received_at", "trigger.automation.name"];
  for (const field of definition.inputsSchema) {
    const key = (field as { key?: unknown }).key;
    if (typeof key === "string") paths.push(`inputs.${key}`);
  }
  if (definition.trigger.kind !== "cron" && definition.trigger.kind !== "manual") {
    paths.push("event.raw");
  }
  for (const block of walkBlocks(definition.blocks)) {
    if (block.id === selectedId) break;
    const spec = blockKind(block.type);
    const outputs = KNOWN_OUTPUTS[spec.kind] ?? [];
    for (const out of outputs) paths.push(`steps.${block.id}.${out}`);
  }
  return paths;
}

const KNOWN_OUTPUTS: Record<string, readonly string[]> = {
  create_session: ["session_id", "task_id"],
  send_prompt: ["outcome", "signal"],
  wait_session: ["outcome"],
  wait_event: ["event", "event_key"],
  run_command: ["exit_status", "stdout", "stderr"],
  write_files: ["files"],
  code: ["value"],
  branch: ["taken"],
  loop: ["iterations"],
  end_session: ["ended"],
  session_status: [
    "found",
    "status",
    "last_active_at",
    "last_event_at",
    "idle_seconds",
    "owner_run_live",
  ],
  state_get: ["found", "value", "version"],
  state_set: ["ok", "version", "current_version", "current_value"],
  state_delete: ["ok", "deleted"],
  state_list: ["entries", "count", "truncated"],
  lookup_pr_session: ["found", "session_id", "task_id", "head_branch", "url", "title"],
  instance_close: ["closed", "not_instanced"],
  claim_handle: ["claimed", "handle"],
  review_open_pass: [
    "review_id",
    "task_id",
    "target_id",
    "head_sha",
    "base_sha",
    "pr_url",
    "superseded_review_id",
    "deduplicated",
  ],
  review_stage: ["phase", "prompt", "merge_base"],
  review_settle: [
    "review_id",
    "repo",
    "pr_number",
    "commit_id",
    "summary_md",
    "comments",
    "to_post_count",
    "ui_only_count",
  ],
  review_close_pass: ["review_id", "outcome"],
};

function sessionSourcesFor(definition: AutomationDefinition, selectedId: string | null): string[] {
  const ids: string[] = [];
  for (const block of walkBlocks(definition.blocks)) {
    if (block.id === selectedId) break;
    if (block.type === "create_session") ids.push(block.id);
  }
  return ids;
}

/** Where a block sits: the list holding it, its index, and that list's
 * length — what Move up / Move down need. */
export function locateBlock(
  blocks: readonly BlockDef[],
  id: string,
  at: ListPath = { root: true },
): { at: ListPath; index: number; length: number } | null {
  for (const [index, block] of blocks.entries()) {
    if (block.id === id) return { at, index, length: blocks.length };
    for (const slot of ["then", "else", "body"] as const) {
      const list = block[slot];
      if (!list) continue;
      const found = locateBlock(list, id, { parentId: block.id, slot });
      if (found) return found;
    }
  }
  return null;
}

export function BuildTab({
  definition,
  entrypointId,
  onSelectEntrypoint,
  onChange,
  builtin,
  errors,
  triggerSummaryFor,
  test,
  testPanel,
  variableValues,
}: BuildTabProps) {
  const ways = entrypointIds(definition);
  const active = projectEntrypoint(definition, entrypointId);
  const firstId = active.blocks[0]?.id ?? TRIGGER_ROW_ID;
  const [selectedId, setSelectedId] = useState<string>(firstId);
  const [zoom, setZoom] = useState<number | "fit">("fit");
  const [adding, setAdding] = useState(false);
  const [draftWay, setDraftWay] = useState("");

  const selected = selectedId === TRIGGER_ROW_ID ? null : findBlock(active.blocks, selectedId);
  // A removed block leaves a dangling selection; fall back to the trigger.
  const effectiveId = selectedId === TRIGGER_ROW_ID || selected ? selectedId : TRIGGER_ROW_ID;

  const erroredIds = useMemo(() => {
    const ids = new Set<string>();
    for (const e of errors) ids.add(e.blockId === "" ? TRIGGER_ROW_ID : e.blockId);
    return ids;
  }, [errors]);
  const blockErrors = errors.filter((e) => e.blockId === effectiveId);
  const triggerErrors = errors.filter((e) => e.blockId === "");

  const changeWay = (way: string, next: AutomationDefinition) =>
    onChange(mergeEntrypoint(definition, way, next));

  const updateBlock = (next: BlockDef) =>
    changeWay(entrypointId, { ...active, blocks: replaceBlock(active.blocks, next.id, next) });
  const updateTrigger = (trigger: TriggerSpec) => changeWay(entrypointId, { ...active, trigger });

  const select = (way: string, id: string) => {
    if (way !== entrypointId) onSelectEntrypoint(way);
    setSelectedId(id);
  };
  const onInsert = (way: string, at: ListPath, index: number, kind: string) => {
    const projected = projectEntrypoint(definition, way);
    const spec = blockKind(kind);
    const id = nextBlockId(projected.blocks, kind, blockIdsOutsideEntrypoint(definition, way));
    const block: BlockDef = { id, type: kind, config: spec.defaults() };
    if (spec.nests === "branch") {
      block.then = [];
      block.else = [];
    }
    if (spec.nests === "loop") block.body = [];
    changeWay(way, { ...projected, blocks: insertBlock(projected.blocks, at, index, block) });
    select(way, id);
  };
  const onMove = (way: string, at: ListPath, from: number, to: number) => {
    const projected = projectEntrypoint(definition, way);
    changeWay(way, { ...projected, blocks: moveBlock(projected.blocks, at, from, to) });
  };
  const onRemove = (way: string, id: string) => {
    const projected = projectEntrypoint(definition, way);
    changeWay(way, { ...projected, blocks: removeBlock(projected.blocks, id) });
    if (selectedId === id) setSelectedId(TRIGGER_ROW_ID);
  };

  const confirmAddWay = () => {
    const id = draftWay.trim();
    if (id === "" || entrypointIdError(definition, id) !== null) return;
    onChange(addEntrypoint(definition, id));
    onSelectEntrypoint(id);
    setSelectedId(TRIGGER_ROW_ID);
    setDraftWay("");
    setAdding(false);
  };
  const removeWay = () => {
    onChange(removeEntrypoint(definition, entrypointId));
    onSelectEntrypoint(MAIN_ENTRYPOINT_ID);
    setSelectedId(TRIGGER_ROW_ID);
  };

  const variablePaths = useMemo(
    () => variablePathsFor(active, selected?.id ?? null),
    [active, selected?.id],
  );
  const sessionSources = useMemo(
    () => sessionSourcesFor(active, selected?.id ?? null),
    [active, selected?.id],
  );
  const position = selected ? locateBlock(active.blocks, selected.id) : null;
  const draftWayError = draftWay === "" ? null : entrypointIdError(definition, draftWay);

  return (
    <div className="flex min-h-0 flex-1" data-testid="build-tab">
      <div className="relative flex min-h-0 min-w-0 flex-1 flex-col">
        {/* The canvas ground: a dot grid in ink at 14%, 20px. */}
        <div
          className="relative flex min-h-0 flex-1 flex-col"
          style={{
            backgroundImage:
              "radial-gradient(color-mix(in oklch, var(--color-foreground) 14%, transparent) 1px, transparent 1px)",
            backgroundSize: "20px 20px",
          }}
        >
          <div className="absolute top-3 right-3 z-30 flex items-center gap-1" aria-label="Zoom">
            <Button
              variant="outline"
              size="sm"
              className="h-7 w-7 px-0"
              aria-label="Zoom out"
              onClick={() => setZoom((z) => Math.max(MIN_SCALE, (z === "fit" ? 1 : z) - 0.1))}
            >
              −
            </Button>
            <Button
              variant="outline"
              size="sm"
              className="h-7 w-7 px-0"
              aria-label="Zoom in"
              onClick={() => setZoom((z) => Math.min(MAX_SCALE, (z === "fit" ? 1 : z) + 0.1))}
            >
              +
            </Button>
            <Button variant="outline" size="sm" className="h-7" onClick={() => setZoom("fit")}>
              Fit
            </Button>
          </div>
          <div className="flex min-h-0 flex-1 gap-6 overflow-x-auto p-6 pt-12">
            {ways.map((way) => {
              const projected = way === entrypointId ? active : projectEntrypoint(definition, way);
              return (
                <div key={way} className="flex min-h-0 min-w-[240px] flex-1 flex-col">
                  <Canvas
                    trigger={projected.trigger}
                    triggerSummary={triggerSummaryFor(way)}
                    entrypointId={ways.length > 1 || way !== MAIN_ENTRYPOINT_ID ? way : undefined}
                    blocks={projected.blocks}
                    selectedId={way === entrypointId ? effectiveId : null}
                    onSelect={(id) => select(way, id)}
                    erroredIds={way === entrypointId ? erroredIds : new Set()}
                    locked={builtin}
                    onInsert={(at, index, kind) => onInsert(way, at, index, kind)}
                    onMove={(at, from, to) => onMove(way, at, from, to)}
                    onRemove={(id) => onRemove(way, id)}
                    zoom={zoom}
                  />
                </div>
              );
            })}
            {!builtin && (
              <div className={cn("flex shrink-0 flex-col", adding ? "w-[264px]" : "w-auto")}>
                {adding ? (
                  <form
                    className="flex flex-col gap-2 rounded-lg border border-dashed bg-card/60 p-3"
                    onSubmit={(e) => {
                      e.preventDefault();
                      confirmAddWay();
                    }}
                  >
                    <Input
                      value={draftWay}
                      onChange={(e) => setDraftWay(e.target.value)}
                      placeholder="review_feedback"
                      aria-label="Name for the new way in"
                      className="font-mono text-xs"
                      autoFocus
                    />
                    {draftWayError && (
                      <p className="text-xs text-muted-foreground">{draftWayError}</p>
                    )}
                    <div className="flex gap-2">
                      <Button type="submit" size="sm" disabled={draftWay === "" || !!draftWayError}>
                        Add
                      </Button>
                      <Button
                        type="button"
                        size="sm"
                        variant="ghost"
                        onClick={() => setAdding(false)}
                      >
                        Cancel
                      </Button>
                    </div>
                  </form>
                ) : (
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    className="h-9 justify-start border border-dashed text-muted-foreground"
                    onClick={() => setAdding(true)}
                    data-testid="add-way-in"
                  >
                    <Plus aria-hidden />
                    Add another way in
                  </Button>
                )}
              </div>
            )}
          </div>
        </div>
        <TestDrawer test={test} panel={testPanel} />
      </div>

      <aside
        className="flex w-[360px] shrink-0 flex-col border-l bg-card"
        aria-label="Inspector"
        data-testid="inspector"
      >
        <div className="flex items-start gap-2 px-[18px] pt-5 pb-3.5">
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <h3 className="truncate text-sm font-semibold">
                {selected ? blockKind(selected.type).label : "Trigger"}
              </h3>
              <span className="rounded-sm bg-secondary px-1.5 py-px font-mono text-2xs text-muted-foreground">
                {selected ? selected.type : ways.length > 1 ? entrypointId : "way in"}
              </span>
            </div>
            <div className="font-mono text-2xs text-muted-foreground">
              {selected ? selected.id : triggerSummaryFor(entrypointId)}
            </div>
          </div>
          {selected && !builtin && position && (
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <Button variant="ghost" size="icon-xs" aria-label={`Actions for ${selected.id}`}>
                  <MoreHorizontal />
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end">
                <DropdownMenuItem
                  disabled={position.index === 0}
                  onSelect={() =>
                    onMove(entrypointId, position.at, position.index, position.index - 1)
                  }
                >
                  <ChevronUp aria-hidden /> Move up
                </DropdownMenuItem>
                <DropdownMenuItem
                  disabled={position.index >= position.length - 1}
                  onSelect={() =>
                    onMove(entrypointId, position.at, position.index, position.index + 1)
                  }
                >
                  <ChevronDown aria-hidden /> Move down
                </DropdownMenuItem>
                <DropdownMenuItem
                  variant="destructive"
                  onSelect={() => onRemove(entrypointId, selected.id)}
                >
                  <Trash2 aria-hidden /> Remove
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          )}
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto px-[18px] pb-4">
          {effectiveId === TRIGGER_ROW_ID ? (
            <TriggerInspector
              trigger={active.trigger}
              onChange={updateTrigger}
              builtin={builtin}
              errors={triggerErrors}
            />
          ) : selected ? (
            <BlockInspector
              key={selected.id}
              block={selected}
              onChange={updateBlock}
              builtin={builtin}
              errors={blockErrors}
              sessionSources={sessionSources}
              variablePaths={variablePaths}
              variableValues={variableValues}
            />
          ) : null}
        </div>
        {!builtin && (
          <div className="flex items-center gap-1 border-t px-3 py-2">
            {selected && position ? (
              <>
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={position.index === 0}
                  onClick={() =>
                    onMove(entrypointId, position.at, position.index, position.index - 1)
                  }
                >
                  <ChevronUp aria-hidden /> Move up
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={position.index >= position.length - 1}
                  onClick={() =>
                    onMove(entrypointId, position.at, position.index, position.index + 1)
                  }
                >
                  <ChevronDown aria-hidden /> Move down
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  className="ml-auto text-destructive hover:text-destructive"
                  onClick={() => onRemove(entrypointId, selected.id)}
                >
                  <Trash2 aria-hidden /> Remove
                </Button>
              </>
            ) : entrypointId !== MAIN_ENTRYPOINT_ID ? (
              <Button
                variant="ghost"
                size="sm"
                className="ml-auto text-destructive hover:text-destructive"
                onClick={removeWay}
                aria-label={`Remove way in ${entrypointId}`}
              >
                <X aria-hidden /> Remove this way in
              </Button>
            ) : null}
          </div>
        )}
      </aside>
    </div>
  );
}

/** The test-with-sample surface as a bottom drawer: a 44px bar that says what
 * the last sample did and offers the two actions, opening to ~40% of the
 * canvas for the full panel. */
function TestDrawer({ test, panel }: { test?: AutomationTestState; panel?: ReactNode }) {
  const [open, setOpen] = useState(false);
  if (!test && !panel) return null;
  const readout = !test
    ? ""
    : test.latest
      ? `${test.latest.blocks.length} block${test.latest.blocks.length === 1 ? "" : "s"} rendered`
      : test.sample.kind === "none"
        ? "no sample picked"
        : "sample picked";
  const canRun = !!test && (test.sample.kind !== "none" || test.isTimed) && !test.running;
  return (
    <div
      className={cn("flex shrink-0 flex-col border-t bg-card", open && "h-[40%] min-h-[220px]")}
      data-testid="test-drawer"
      data-state={open ? "open" : "closed"}
    >
      <div className="flex h-11 shrink-0 items-center gap-3 px-3">
        <button
          type="button"
          className="flex items-center gap-2 text-sm font-semibold"
          onClick={() => setOpen((v) => !v)}
          aria-expanded={open}
        >
          <ChevronUp
            className={cn(
              "size-4 text-muted-foreground transition-transform",
              open && "rotate-180",
            )}
            aria-hidden
          />
          Test with a sample
        </button>
        <span className="truncate text-xs text-muted-foreground">{readout}</span>
        <div className="ml-auto flex items-center gap-1">
          <Button variant="ghost" size="sm" onClick={() => setOpen(true)}>
            Pick sample
          </Button>
          {test && (
            <Button
              variant="outline"
              size="sm"
              disabled={!canRun}
              onClick={() => void test.runOnce()}
            >
              Run test
            </Button>
          )}
        </div>
      </div>
      {open && (
        <div className="min-h-0 flex-1 overflow-y-auto px-3 pb-3">
          {panel ?? (test ? <TestPanel test={test} /> : null)}
        </div>
      )}
    </div>
  );
}
