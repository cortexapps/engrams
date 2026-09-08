/** The Build tab (ADR 0119 phase 3.3): block list left, inspector right.
 *
 * Owns nothing durable — the shell holds the draft definition and passes
 * change callbacks down; this component is layout + selection. The
 * `testPanel` slot is where 3.4 mounts "Test with sample" results. */

import { useMemo, useState, type ReactNode } from "react";

import { ResizableHandle, ResizablePanel, ResizablePanelGroup } from "@/components/ui/resizable";
import {
  blockKind,
  findBlock,
  insertBlock,
  moveBlock,
  nextBlockId,
  removeBlock,
  replaceBlock,
  walkBlocks,
  type AutomationDefinition,
  type BlockDef,
  type BlockErrorRef,
  type ListPath,
  type TriggerSpec,
} from "@/lib/automation-blocks";

import { BlockInspector } from "./BlockInspector";
import { Canvas, TRIGGER_ROW_ID } from "./canvas/Canvas";
import { TriggerInspector } from "./TriggerInspector";

export interface BuildTabProps {
  definition: AutomationDefinition;
  onChange: (next: AutomationDefinition) => void;
  builtin: boolean;
  errors: readonly BlockErrorRef[];
  triggerSummary: string;
  /** 3.4: TestPanel mounts here (below the inspector). */
  testPanel?: ReactNode;
  /** 3.4: live variable values keyed by path for the selected sample. */
  variableValues?: Readonly<Record<string, string>>;
  /** D9: block ids in OTHER entrypoints — ids stay unique automation-wide. */
  reservedBlockIds?: readonly string[];
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
};

function sessionSourcesFor(definition: AutomationDefinition, selectedId: string | null): string[] {
  const ids: string[] = [];
  for (const block of walkBlocks(definition.blocks)) {
    if (block.id === selectedId) break;
    if (block.type === "create_session") ids.push(block.id);
  }
  return ids;
}

export function BuildTab({
  definition,
  onChange,
  builtin,
  errors,
  triggerSummary,
  testPanel,
  variableValues,
  reservedBlockIds,
}: BuildTabProps) {
  const firstId = definition.blocks[0]?.id ?? TRIGGER_ROW_ID;
  const [selectedId, setSelectedId] = useState<string>(firstId);
  const selected = selectedId === TRIGGER_ROW_ID ? null : findBlock(definition.blocks, selectedId);
  // A removed block leaves a dangling selection; fall back to the trigger.
  const effectiveId = selectedId === TRIGGER_ROW_ID || selected ? selectedId : TRIGGER_ROW_ID;

  const erroredIds = useMemo(() => {
    const ids = new Set<string>();
    for (const e of errors) ids.add(e.blockId === "" ? TRIGGER_ROW_ID : e.blockId);
    return ids;
  }, [errors]);
  const blockErrors = errors.filter((e) => e.blockId === effectiveId);
  const triggerErrors = errors.filter((e) => e.blockId === "");

  const updateBlock = (next: BlockDef) =>
    onChange({
      ...definition,
      blocks: replaceBlock(definition.blocks, next.id, next),
    });
  const updateTrigger = (trigger: TriggerSpec) => onChange({ ...definition, trigger });
  const onInsert = (at: ListPath, index: number, kind: string) => {
    const spec = blockKind(kind);
    const id = nextBlockId(definition.blocks, kind, reservedBlockIds ?? []);
    const block: BlockDef = { id, type: kind, config: spec.defaults() };
    if (spec.nests === "branch") {
      block.then = [];
      block.else = [];
    }
    if (spec.nests === "loop") block.body = [];
    onChange({
      ...definition,
      blocks: insertBlock(definition.blocks, at, index, block),
    });
    setSelectedId(id);
  };
  const onMove = (at: ListPath, from: number, to: number) =>
    onChange({
      ...definition,
      blocks: moveBlock(definition.blocks, at, from, to),
    });
  const onRemove = (id: string) => {
    onChange({ ...definition, blocks: removeBlock(definition.blocks, id) });
    if (selectedId === id) setSelectedId(TRIGGER_ROW_ID);
  };

  const variablePaths = useMemo(
    () => variablePathsFor(definition, selected?.id ?? null),
    [definition, selected?.id],
  );
  const sessionSources = useMemo(
    () => sessionSourcesFor(definition, selected?.id ?? null),
    [definition, selected?.id],
  );

  return (
    <ResizablePanelGroup orientation="horizontal" className="min-h-[480px] rounded-lg border">
      <ResizablePanel defaultSize={42} minSize={28}>
        <Canvas
          trigger={definition.trigger}
          triggerSummary={triggerSummary}
          blocks={definition.blocks}
          selectedId={effectiveId}
          onSelect={setSelectedId}
          erroredIds={erroredIds}
          locked={builtin}
          onInsert={onInsert}
          onMove={onMove}
          onRemove={onRemove}
        />
      </ResizablePanel>
      <ResizableHandle />
      <ResizablePanel defaultSize={58} minSize={32}>
        <div className="flex h-full flex-col gap-4 overflow-y-auto p-4">
          {effectiveId === TRIGGER_ROW_ID ? (
            <TriggerInspector
              trigger={definition.trigger}
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
          {testPanel}
        </div>
      </ResizablePanel>
    </ResizablePanelGroup>
  );
}
