/** The Build tab: the canvas (one column per way in, every column ending on
 * the automation's one Agent node) with the inspector as a side sheet on
 * the right and the test drawer along the bottom. The seam between the
 * canvas and the inspector is a drag handle, because the inspector holds the
 * code editor and a fixed 360px is too narrow to read code in.
 *
 * Owns nothing durable — the shell holds the draft definition and passes
 * change callbacks down; this component is layout + selection. Block ids are
 * unique automation-wide, so selecting a node in any column also selects that
 * column's way in for the inspector; the agent node selects the agent. */

import { ChevronDown, ChevronUp, MoreHorizontal, Plus, Trash2, X } from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";

import type { AutomationRunBrief } from "@/gen/engram/app/v1/automation_pb";

import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { ResizableHandle, ResizablePanel, ResizablePanelGroup } from "@/components/ui/resizable";
import { useEventCatalog } from "@/hooks/useAutomationEditor";
import { useProfiles } from "@/hooks/useProfiles";
import { flattenScope, type AutomationTestState } from "@/hooks/useAutomationTest";
import {
  addEntrypoint,
  AGENT_CLOSE_STEP_ID,
  AGENT_NODE_ID,
  AGENT_STEP_ID,
  blockIdsOutsideEntrypoint,
  blockKind,
  blockProfileId,
  defaultAgent,
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
  roleOf,
  scopeLabel,
  setRole,
  walkBlocks,
  type AgentSpec,
  type AutomationDefinition,
  type BlockDef,
  type BlockErrorRef,
  type ListPath,
  type TriggerSpec,
} from "@/lib/automation-blocks";
import { mergePaths, schemaPaths } from "@/lib/event-paths";
import { relativeAge } from "@/lib/relative-time";
import { wayOnPath, type ReplayMap } from "@/lib/run-replay";
import { cn } from "@/lib/utils";

import { entryTitle } from "../activity/activity-format";
import { RunStatusDot } from "../runs/RunStatusDot";
import { formatDuration, parseJsonObject } from "../runs/run-format";
import { StepDetails } from "../runs/StepDetails";

import { BlockInspector } from "./BlockInspector";
import { AddAgentNode, AgentBus, AgentNode, AgentStub } from "./canvas/AgentNode";
import { Canvas, TRIGGER_ROW_ID } from "./canvas/Canvas";
import { MAX_SCALE, MIN_SCALE } from "./canvas/layout";
import { AgentInspector } from "./inspectors/AgentInspector";
import { TestPanel } from "./test/TestPanel";
import { TriggerInspector } from "./TriggerInspector";

/** A past run to draw on the canvas instead of editing it. */
export interface RunReplay {
  run: AutomationRunBrief;
  map: ReplayMap;
  /** The delivery that started the run, for the trigger node's details. */
  triggerJson: string;
  /** Set when the run executed a version other than the one drawn. */
  versionNote?: string;
  now: number;
  onExit: () => void;
}

export interface BuildTabProps {
  /** The whole automation, every way in included. In replay, the version
   * the run executed. */
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
  /** The test drawer, open or closed; the header's Try it opens it. */
  testOpen?: boolean;
  onTestOpenChange?: (open: boolean) => void;
  /** Replay a run on the canvas: nodes say what they did, the untravelled
   * parts fade, a node click shows its inputs and outputs. */
  replay?: RunReplay;
}

/** The trigger's event, as the catalog describes it: the first declared key
 * of an integration trigger. */
function catalogEventFor(trigger: TriggerSpec): { provider: string; key: string } | null {
  if (trigger.kind !== "integration") return null;
  const provider = typeof trigger["provider"] === "string" ? trigger["provider"] : "";
  const keys = Array.isArray(trigger["eventKeys"]) ? (trigger["eventKeys"] as string[]) : [];
  const key = keys[0];
  return provider && key ? { provider, key } : null;
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
    for (const out of blockKind(block.type).outputs ?? []) {
      paths.push(`steps.${block.id}.${out}`);
    }
  }
  return paths;
}

function sessionSourcesFor(definition: AutomationDefinition, selectedId: string | null): string[] {
  const ids: string[] = [];
  for (const block of walkBlocks(definition.blocks)) {
    if (block.id === selectedId) break;
    if (block.type === "create_session") ids.push(block.id);
  }
  return ids;
}

/** The agent without its ways in's roles: what removing it leaves. */
function withoutAgent(definition: AutomationDefinition): AutomationDefinition {
  const { agent: _agent, role: _role, ...rest } = definition;
  return {
    ...rest,
    ...(rest.entrypoints
      ? {
          entrypoints: rest.entrypoints.map((ep) => {
            const { role: _epRole, ...way } = ep;
            return way;
          }),
        }
      : {}),
  };
}

/**
 * How wide the inspector opens, as a percentage of the Build tab.
 *
 * A pane width is a personal, per-browser layout preference (like the rail
 * widths and the work pane's split), so it lives in localStorage rather than on
 * the server. react-resizable-panels reports a layout in percent, so percent is
 * what we keep; with nothing stored the inspector opens at the designed 360px.
 */
const INSPECTOR_WIDTH_KEY = "engram.automation-inspector";
const INSPECTOR_DEFAULT_WIDTH = "360px";
/** Floors that keep both halves usable however the seam is dragged: one canvas
 * column plus its padding on the left, a readable form/code field on the
 * right. The canvas floor is also what caps the inspector. */
const CANVAS_MIN_WIDTH = "280px";
const INSPECTOR_MIN_WIDTH = "320px";

function readInspectorWidth(): number | null {
  try {
    const stored = Number(localStorage.getItem(INSPECTOR_WIDTH_KEY));
    // A missing or garbled value reads as 0/NaN, and a full-width or wider
    // reading is not a split at all — keep the px default for both.
    return Number.isFinite(stored) && stored > 0 && stored < 100 ? stored : null;
  } catch {
    return null;
  }
}

function writeInspectorWidth(percent: number) {
  try {
    localStorage.setItem(INSPECTOR_WIDTH_KEY, String(percent));
  } catch {
    // ignore — persistence is best-effort
  }
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
  testOpen,
  onTestOpenChange,
  replay,
}: BuildTabProps) {
  const ways = entrypointIds(definition);
  const active = projectEntrypoint(definition, entrypointId);
  const firstId = active.blocks[0]?.id ?? TRIGGER_ROW_ID;
  const [selectedId, setSelectedId] = useState<string>(firstId);
  const [zoom, setZoom] = useState<number | "fit">("fit");
  const [adding, setAdding] = useState(false);
  const [draftWay, setDraftWay] = useState("");
  // Read once: `defaultSize` is only consulted on mount, and every later width
  // belongs to the library and the store, not to a render.
  const [storedInspectorWidth] = useState(readInspectorWidth);
  // Replay opens on the step that matters: the one that failed or stopped
  // the run, else the last one. A looped block shows the frame picked here.
  const [framePath, setFramePath] = useState<string | null>(null);
  const replayRunId = replay?.run.id;
  useEffect(() => {
    if (!replay) return;
    const nodes = [...replay.map.values()];
    const focus =
      nodes.find((n) => n.status === "failed" || n.status === "skipped") ?? nodes.at(-1);
    setFramePath(null);
    if (focus && (focus.blockId === AGENT_STEP_ID || focus.blockId === AGENT_CLOSE_STEP_ID)) {
      setSelectedId(AGENT_NODE_ID);
    } else if (focus) {
      const way = ways.find((w) =>
        [...walkBlocks(projectEntrypoint(definition, w).blocks)].some(
          (b) => b.id === focus.blockId,
        ),
      );
      if (way && way !== entrypointId) onSelectEntrypoint(way);
      setSelectedId(focus.blockId);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- only a new run re-focuses.
  }, [replayRunId]);

  const agent = definition.agent;
  const agentSelected = selectedId === AGENT_NODE_ID && agent !== undefined;
  const selected =
    selectedId === TRIGGER_ROW_ID || selectedId === AGENT_NODE_ID
      ? null
      : findBlock(active.blocks, selectedId);
  // A removed block leaves a dangling selection; fall back to the trigger.
  const effectiveId =
    selectedId === TRIGGER_ROW_ID || selected || agentSelected ? selectedId : TRIGGER_ROW_ID;

  const erroredIds = useMemo(() => {
    const ids = new Set<string>();
    for (const e of errors) ids.add(e.blockId === "" ? TRIGGER_ROW_ID : e.blockId);
    return ids;
  }, [errors]);
  const blockErrors = errors.filter((e) => e.blockId === effectiveId);
  const triggerErrors = errors.filter((e) => e.blockId === "");
  const agentErrors = errors.filter((e) => e.blockId === AGENT_NODE_ID);

  const changeWay = (way: string, next: AutomationDefinition) =>
    onChange(mergeEntrypoint(definition, way, next));

  const updateBlock = (next: BlockDef) =>
    changeWay(entrypointId, { ...active, blocks: replaceBlock(active.blocks, next.id, next) });
  const updateTrigger = (trigger: TriggerSpec) => changeWay(entrypointId, { ...active, trigger });
  const updateAgent = (next: AgentSpec) => onChange({ ...definition, agent: next });
  const addAgent = () => {
    onChange({ ...definition, agent: defaultAgent(definition.trigger) });
    setSelectedId(AGENT_NODE_ID);
  };
  const removeAgent = () => {
    onChange(withoutAgent(definition));
    setSelectedId(TRIGGER_ROW_ID);
  };

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

  // The event's fields come from the connector's catalog: its curated schema
  // names the paths, its newest sample previews them before any test runs.
  const catalogEvent = catalogEventFor(active.trigger);
  const catalog = useEventCatalog(catalogEvent?.provider);
  const catalogEntry = catalog.data?.events.find((event) => event.key === catalogEvent?.key);
  const eventPaths = useMemo(
    () => schemaPaths(catalogEntry?.schemaJson ?? ""),
    [catalogEntry?.schemaJson],
  );
  const previewValues = useMemo(() => {
    const values: Record<string, string> = {};
    if (catalogEntry?.sampleJson) {
      try {
        flattenScope({ event: { raw: JSON.parse(catalogEntry.sampleJson) as unknown } }, values);
      } catch {
        // a sample that does not parse previews nothing
      }
    }
    return { ...values, ...variableValues };
  }, [catalogEntry?.sampleJson, variableValues]);
  const variablePaths = useMemo(
    () =>
      mergePaths([...variablePathsFor(active, selected?.id ?? null), ...eventPaths], previewValues),
    [active, selected?.id, eventPaths, previewValues],
  );
  const sessionSources = useMemo(
    () => sessionSourcesFor(active, selected?.id ?? null),
    [active, selected?.id],
  );
  const position = selected ? locateBlock(active.blocks, selected.id) : null;
  const draftWayError = draftWay === "" ? null : entrypointIdError(definition, draftWay);

  // A node names its profile, not the profile's id.
  const profiles = useProfiles();
  const summaryFor = (block: BlockDef): string | undefined => {
    const id = blockProfileId(block);
    if (id === undefined) return undefined;
    const name = profiles.data?.profiles.find((p) => p.id === id)?.name;
    if (name === undefined) return undefined;
    return blockKind(block.type).summary({ ...block.config, profileId: name });
  };

  const replayAgent = replay
    ? (replay.map.get(AGENT_STEP_ID) ?? replay.map.get(AGENT_CLOSE_STEP_ID))
    : undefined;
  const replayNode = replay
    ? selected
      ? replay.map.get(selected.id)
      : agentSelected
        ? replayAgent
        : undefined
    : undefined;
  const replayStep =
    replayNode && (replayNode.frames.find((f) => f.path === framePath) ?? replayNode.latest);
  // A way in carried the run when one of its blocks ran, or when the run
  // names it (a way in with no blocks of its own runs only the agent step).
  const wayCarried = (way: string, blocks: readonly BlockDef[]): boolean =>
    !!replay &&
    (replay.run.entrypointId === way ||
      wayOnPath(
        replay.map,
        [...walkBlocks(blocks)].map((b) => b.id),
      ));
  const agentSummary = (() => {
    if (!agent) return "";
    const name = profiles.data?.profiles.find((p) => p.id === agent.profileId)?.name;
    const who = name ?? (agent.profileId ? agent.profileId : "No profile selected");
    const scope = scopeLabel(definition) ?? "a session per event";
    return `${who} · ${scope}`;
  })();

  return (
    <ResizablePanelGroup
      orientation="horizontal"
      className="min-h-0 flex-1"
      data-testid="build-tab"
      data-replay={replay ? replay.run.id : undefined}
      // `onLayoutChanged` (past tense) fires once the pointer is released
      // rather than on every drag frame — upstream's guidance for writing to a
      // storage API. Only remember a genuine split; a degenerate reading would
      // clobber the preferred width.
      onLayoutChanged={(layout) => {
        const { "build-canvas": canvas, "build-inspector": inspector } = layout;
        if (canvas > 1 && inspector > 1) writeInspectorWidth(inspector);
      }}
    >
      <ResizablePanel id="build-canvas" minSize={CANVAS_MIN_WIDTH} className="min-w-0">
        <div className="relative flex h-full min-h-0 min-w-0 flex-col">
          {replay && (
            <div
              className="flex shrink-0 flex-wrap items-center gap-x-3 gap-y-1 border-b bg-card px-4 py-2 text-xs"
              data-testid="replay-banner"
            >
              <RunStatusDot status={replay.run.status} />
              <span className="font-semibold">
                Replaying {entryTitle(replay.run).toLowerCase()}
              </span>
              <span className="text-muted-foreground">
                {relativeAge(replay.run.createdAt, replay.now)} ago ·{" "}
                {replay.run.status.replaceAll("_", " ")} ·{" "}
                <span className="font-mono tabular-nums">
                  {formatDuration(replay.run.startedAt, replay.run.endedAt, replay.now)}
                </span>
              </span>
              {replay.run.error && (
                <span className="min-w-0 truncate text-foreground">
                  {replay.run.error.split("\n")[0]}
                </span>
              )}
              {replay.versionNote && (
                <span className="text-muted-foreground" data-testid="replay-version">
                  {replay.versionNote}
                </span>
              )}
              <Button
                variant="outline"
                size="sm"
                className="ml-auto h-7"
                onClick={replay.onExit}
                data-testid="replay-exit"
              >
                Back to editing
              </Button>
            </div>
          )}
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
              <div className="flex min-h-0 min-w-0 flex-1 flex-col">
                <div className="flex min-h-0 flex-1 gap-6">
                  {ways.map((way) => {
                    const projected =
                      way === entrypointId ? active : projectEntrypoint(definition, way);
                    const carried = wayCarried(way, projected.blocks);
                    return (
                      <div key={way} className="flex min-h-0 min-w-[240px] flex-1 flex-col">
                        <Canvas
                          trigger={projected.trigger}
                          triggerSummary={triggerSummaryFor(way)}
                          entrypointId={
                            ways.length > 1 || way !== MAIN_ENTRYPOINT_ID ? way : undefined
                          }
                          blocks={projected.blocks}
                          selectedId={way === entrypointId ? effectiveId : null}
                          onSelect={(id) => select(way, id)}
                          erroredIds={way === entrypointId ? erroredIds : new Set()}
                          locked={builtin}
                          onInsert={(at, index, kind) => onInsert(way, at, index, kind)}
                          onMove={(at, from, to) => onMove(way, at, from, to)}
                          onRemove={(id) => onRemove(way, id)}
                          zoom={zoom}
                          summaryFor={summaryFor}
                          {...(replay ? { replay: { map: replay.map, onPath: carried } } : {})}
                        />
                        {agent && (
                          <AgentStub
                            role={roleOf(definition, way)}
                            dimmed={replay ? !carried : false}
                          />
                        )}
                      </div>
                    );
                  })}
                </div>
                {agent ? (
                  <AgentBus columns={ways.length}>
                    <AgentNode
                      summary={agentSummary}
                      selected={agentSelected}
                      errored={agentErrors.length > 0}
                      {...(replayAgent ? { status: replayAgent.status } : {})}
                      dimmed={replay ? !replayAgent : false}
                      onSelect={() => setSelectedId(AGENT_NODE_ID)}
                    />
                  </AgentBus>
                ) : (
                  !builtin &&
                  !replay && (
                    <div className="flex shrink-0 justify-center pt-4">
                      <AddAgentNode onAdd={addAgent} />
                    </div>
                  )
                )}
              </div>
              {!builtin && !replay && (
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
                        <Button
                          type="submit"
                          size="sm"
                          disabled={draftWay === "" || !!draftWayError}
                        >
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
          {!replay && (
            <TestDrawer
              test={test}
              panel={testPanel}
              open={testOpen}
              onOpenChange={onTestOpenChange}
            />
          )}
        </div>
      </ResizablePanel>

      {/* The seam the inspector is dragged by. The pill is the handle's own
          look — the same control as the rail's on the other side of the page. */}
      <ResizableHandle />

      <ResizablePanel
        id="build-inspector"
        defaultSize={storedInspectorWidth ?? INSPECTOR_DEFAULT_WIDTH}
        minSize={INSPECTOR_MIN_WIDTH}
        className="min-w-0"
      >
        <aside
          className="flex h-full flex-col border-l bg-card"
          aria-label="Inspector"
          data-testid="inspector"
        >
          <div className="flex items-start gap-2 px-[18px] pt-5 pb-3.5">
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2">
                <h3 className="truncate text-sm font-semibold">
                  {selected ? blockKind(selected.type).label : agentSelected ? "Agent" : "Trigger"}
                </h3>
                <span className="rounded-sm bg-secondary px-1.5 py-px font-mono text-2xs text-muted-foreground">
                  {selected
                    ? selected.type
                    : agentSelected
                      ? "agent"
                      : ways.length > 1
                        ? entrypointId
                        : "way in"}
                </span>
              </div>
              <div className="font-mono text-2xs text-muted-foreground">
                {selected
                  ? selected.id
                  : agentSelected
                    ? agentSummary
                    : triggerSummaryFor(entrypointId)}
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
            {replay ? (
              effectiveId === TRIGGER_ROW_ID ? (
                <section className="flex flex-col gap-1" data-testid="replay-delivery">
                  <h4 className="text-xs font-semibold text-muted-foreground">
                    The delivery that started this run
                  </h4>
                  <pre className="max-h-96 overflow-auto rounded-md bg-secondary p-3 font-mono text-xs">
                    {(() => {
                      const parsed = parseJsonObject(replay.triggerJson);
                      return parsed ? JSON.stringify(parsed, null, 2) : replay.triggerJson || "—";
                    })()}
                  </pre>
                </section>
              ) : replayNode && replayStep ? (
                <StepDetails
                  step={replayStep}
                  blockType={
                    selected?.type ??
                    (replayNode.blockId === AGENT_CLOSE_STEP_ID ? "instance_close" : "run_agent")
                  }
                  sentence={replayNode.sentence}
                  frames={replayNode.frames}
                  onPickFrame={setFramePath}
                />
              ) : (
                <p className="text-sm text-muted-foreground" data-testid="replay-off-path">
                  {agentSelected
                    ? "That run never reached the agent."
                    : "This block was not on the path of that run."}
                </p>
              )
            ) : effectiveId === TRIGGER_ROW_ID ? (
              <TriggerInspector
                trigger={active.trigger}
                onChange={updateTrigger}
                builtin={builtin}
                errors={triggerErrors}
                {...(agent
                  ? {
                      role: roleOf(definition, entrypointId),
                      onRoleChange: (role) => onChange(setRole(definition, entrypointId, role)),
                    }
                  : {})}
              />
            ) : agentSelected && agent ? (
              <AgentInspector
                agent={agent}
                onChange={updateAgent}
                builtin={builtin}
                errors={agentErrors}
                variablePaths={variablePaths}
                variableValues={previewValues}
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
                variableValues={previewValues}
              />
            ) : null}
          </div>
          {!builtin && !replay && (
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
              ) : agentSelected ? (
                <Button
                  variant="ghost"
                  size="sm"
                  className="ml-auto text-destructive hover:text-destructive"
                  onClick={removeAgent}
                  aria-label="Remove the agent"
                >
                  <X aria-hidden /> Remove the agent
                </Button>
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
      </ResizablePanel>
    </ResizablePanelGroup>
  );
}

/** The test-with-sample surface as a bottom drawer: a 44px bar that says what
 * the last sample did and offers the two actions, opening to ~40% of the
 * canvas for the full panel. */
function TestDrawer({
  test,
  panel,
  open: controlledOpen,
  onOpenChange,
}: {
  test?: AutomationTestState;
  panel?: ReactNode;
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
}) {
  const [localOpen, setLocalOpen] = useState(false);
  const open = controlledOpen ?? localOpen;
  const setOpen = (next: boolean | ((prev: boolean) => boolean)) => {
    const value = typeof next === "function" ? next(open) : next;
    setLocalOpen(value);
    onOpenChange?.(value);
  };
  if (!test && !panel) return null;
  const readout = !test
    ? ""
    : test.latest
      ? `${test.latest.blocks.length} block${test.latest.blocks.length === 1 ? "" : "s"} rendered`
      : test.sample.kind === "none"
        ? "no sample picked"
        : test.sample.kind === "latest"
          ? "the latest delivery"
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
