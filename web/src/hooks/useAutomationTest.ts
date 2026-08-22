/** "Test with sample" state + runner (ADR 0119 phase 3.4).
 *
 * One piece of editor state — the selected sample — rides every TestRender
 * (and, for 3.5, EvalCode/DryRun) call. The latest render's per-block
 * `scope_json` is flattened into live variable previews for the picker.
 */

import { useCallback, useMemo, useState } from "react";
import { useMutation } from "@connectrpc/connect-query";

import { testRender } from "@/gen/engram/app/v1/automation-AutomationService_connectquery";
import type { EventSample, TestRenderResponse } from "@/gen/engram/app/v1/automation_pb";
import { useEventSamples } from "@/hooks/useAutomations";
import type { AutomationDefinition, BlockErrorRef } from "@/lib/automation-blocks";

/** What the current trigger can be tested against. */
export type TestSample =
  | { kind: "none" }
  | { kind: "sample"; sampleId: string }
  | { kind: "payload"; payloadJson: string }
  | { kind: "scheduled"; scheduledFor: string };

export interface BlockRenderResult {
  blockId: string;
  blockType: string;
  rendered: unknown;
  /** Present for filter/branch blocks. */
  filterPass?: boolean;
  scope: Record<string, unknown>;
}

export interface TestRenderResult {
  blocks: BlockRenderResult[];
  errors: BlockErrorRef[];
}

/** Tally of one filter block's verdict across recent samples. */
export interface FilterTally {
  blockId: string;
  passed: number;
  total: number;
}

function parseJson(text: string): unknown {
  if (!text) return undefined;
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return text;
  }
}

function toResult(response: TestRenderResponse): TestRenderResult {
  return {
    blocks: response.blocks.map((b) => ({
      blockId: b.blockId,
      blockType: b.blockType,
      rendered: parseJson(b.renderedJson),
      ...(b.filterPass !== undefined ? { filterPass: b.filterPass } : {}),
      scope: (parseJson(b.scopeJson) as Record<string, unknown> | undefined) ?? {},
    })),
    errors: response.errors.map((e) => ({
      blockId: e.blockId,
      field: e.field,
      message: e.message,
    })),
  };
}

const PREVIEW_MAX_CHARS = 80;

function previewOf(value: unknown): string {
  if (value === undefined) return "";
  const text = typeof value === "string" ? value : JSON.stringify(value);
  if (text === undefined) return "";
  return text.length > PREVIEW_MAX_CHARS ? `${text.slice(0, PREVIEW_MAX_CHARS - 1)}…` : text;
}

const FLATTEN_MAX_DEPTH = 6;
const FLATTEN_MAX_PATHS = 500;

/** Flatten a render scope into `path → preview` for the variable picker.
 * Objects recurse (own keys only, bounded depth/width); every node also gets
 * an entry for itself so `event.raw` and `event.raw.issue.title` both
 * preview. Arrays preview whole. */
export function flattenScope(
  scope: Record<string, unknown>,
  into: Record<string, string> = {},
): Record<string, string> {
  let count = 0;
  const walk = (value: unknown, path: string, depth: number) => {
    if (count >= FLATTEN_MAX_PATHS) return;
    if (path !== "") {
      into[path] = previewOf(value);
      count += 1;
    }
    if (
      depth >= FLATTEN_MAX_DEPTH ||
      typeof value !== "object" ||
      value === null ||
      Array.isArray(value)
    ) {
      return;
    }
    for (const key of Object.keys(value as Record<string, unknown>)) {
      walk(
        (value as Record<string, unknown>)[key],
        path === "" ? key : `${path}.${key}`,
        depth + 1,
      );
    }
  };
  walk(scope, "", 0);
  return into;
}

/** The picker's live values: the scope as seen by the LAST block that ran
 * (it carries every earlier block's outputs), merged over earlier blocks so
 * a filtered run still previews what was available before the filter. */
export function variableValuesFrom(result: TestRenderResult | null): Record<string, string> {
  if (!result) return {};
  const values: Record<string, string> = {};
  for (const block of result.blocks) flattenScope(block.scope, values);
  return values;
}

export interface UseAutomationTestOptions {
  automationId: string | undefined;
  definition: AutomationDefinition;
  inputsJson?: string;
  /** Server errors from a render route into the inspector via the shell. */
  onErrors?: (errors: BlockErrorRef[]) => void;
}

export function useAutomationTest({
  automationId,
  definition,
  inputsJson = "{}",
  onErrors,
}: UseAutomationTestOptions) {
  const isTimed = definition.trigger.kind === "cron" || definition.trigger.kind === "manual";
  const samples = useEventSamples(isTimed ? undefined : automationId, 20);
  const sampleList: EventSample[] = samples.data?.samples ?? [];

  const [sample, setSample] = useState<TestSample>({ kind: "none" });
  const [latest, setLatest] = useState<TestRenderResult | null>(null);
  const [tally, setTally] = useState<FilterTally[] | null>(null);
  const render = useMutation(testRender);

  const requestFor = useCallback(
    (target: TestSample) => ({
      automationId: automationId ?? "",
      draftDefinitionJson: JSON.stringify(definition),
      inputsJson,
      ...(target.kind === "sample"
        ? { sample: { case: "sampleId" as const, value: target.sampleId } }
        : target.kind === "payload"
          ? { sample: { case: "payloadJson" as const, value: target.payloadJson } }
          : {}),
      ...(target.kind === "scheduled" ? { scheduledFor: target.scheduledFor } : {}),
    }),
    [automationId, definition, inputsJson],
  );

  /** Render the draft against the selected sample. */
  const runOnce = useCallback(async (): Promise<TestRenderResult | null> => {
    if (!automationId) return null;
    const response = await render.mutateAsync(requestFor(sample));
    const result = toResult(response);
    setLatest(result);
    setTally(null);
    onErrors?.(result.errors);
    return result;
  }, [automationId, render, requestFor, sample, onErrors]);

  /** Render across the stored samples and tally each filter/branch verdict —
   * the "14/20 pass" affordance. The latest single render is replaced by the
   * most recent sample's so the picker previews stay live. */
  const runAcrossSamples = useCallback(async (): Promise<FilterTally[] | null> => {
    if (!automationId || sampleList.length === 0) return null;
    const counts = new Map<string, FilterTally>();
    let first: TestRenderResult | null = null;
    for (const s of sampleList) {
      const result = toResult(
        await render.mutateAsync(requestFor({ kind: "sample", sampleId: s.id })),
      );
      first ??= result;
      for (const block of result.blocks) {
        if (block.filterPass === undefined) continue;
        const entry = counts.get(block.blockId) ?? { blockId: block.blockId, passed: 0, total: 0 };
        entry.total += 1;
        if (block.filterPass) entry.passed += 1;
        counts.set(block.blockId, entry);
      }
    }
    const tallies = [...counts.values()];
    setTally(tallies);
    if (first) {
      setLatest(first);
      onErrors?.(first.errors);
    }
    return tallies;
  }, [automationId, sampleList, render, requestFor, onErrors]);

  const variableValues = useMemo(() => variableValuesFrom(latest), [latest]);

  return {
    isTimed,
    samples: sampleList,
    samplesLoading: samples.isLoading,
    sample,
    setSample,
    latest,
    tally,
    running: render.isPending,
    runOnce,
    runAcrossSamples,
    variableValues,
  };
}

export type AutomationTestState = ReturnType<typeof useAutomationTest>;
