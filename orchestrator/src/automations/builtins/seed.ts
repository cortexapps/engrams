/** Boot-time seeding of built-in automations (ADR 0119 D7).
 *
 * The seed-profile pattern: idempotent, unique-violation tolerant across
 * replicas, fire-and-forget at boot after the default connections exist.
 *
 * A built-in is created DISABLED: the parallel-run window opens per repo or
 * channel through its inputs and the per-surface flags (4.4/4.6), never by
 * a deploy.
 *
 * On a later boot the shipped definition may have changed. Structural
 * changes are ours alone (built-ins are structure-locked), so a content-hash
 * mismatch simply inserts the next version and repoints; what must survive a
 * bump is the org's configuration: input values are merged with NEW-key
 * defaults only (an org value is never overwritten), and block overrides are
 * reconciled with the three-way rule in mergeBuiltinOverrides (an override
 * equal to the old shipped default was never an edit and follows the new
 * default; a real edit is kept).
 */

import { createHash } from "node:crypto";

import { makeAutomationStore, type AutomationRow, type AutomationStore } from "../../db/automations.ts";
import { makeEnrollmentStore } from "../../db/enrollments.ts";
import { makeIntegrationConnectionStore } from "../../db/integration-connections.ts";
import { isUniqueViolation } from "../../db/pg-errors.ts";
import { log as rootLog } from "../../log.ts";
import { overrideTargets } from "../engine/definition.ts";
import type { AutomationDefinition, BlockDef, BlockOverrides, InputFieldSpec } from "../engine/definition.ts";
import { validateInputValues } from "../inputs.ts";
import {
  listBuiltinAutomations,
  mergeBuiltinOverrides,
  registerBuiltinAutomation,
  type BuiltinAutomation,
} from "../engine/builtins.ts";
import { DEFAULT_CONNECTION_PLACEHOLDER, PR_REVIEW_BUILTIN, PR_REVIEW_BUILTIN_KEY } from "./pr-review.ts";

const log = rootLog.child({ component: "builtin-seed" });

export type BuiltinSeedStore = Pick<
  AutomationStore,
  "getByBuiltinKey" | "create" | "saveVersion" | "setInputs" | "setBlockOverrides"
>;

export interface BuiltinSeedDeps {
  store: BuiltinSeedStore;
  connections: {
    ensureDefault(provider: string, displayName: string): Promise<{ id: string }>;
  };
  /** The review enrollment lift (first seed of pr_review only). Legacy table;
   * deleted in phase 4.7 along with this seam. */
  enrollments?: {
    list(): Promise<Array<{ repo: string; triggerMode: string; autofix: string }>>;
  };
  builtins?: BuiltinAutomation[];
  log?: { info(b: Record<string, unknown>, m: string): void; warn(b: Record<string, unknown>, m: string): void };
}

export interface BuiltinSeedResult {
  created: string[];
  bumped: Array<{ key: string; from: number; to: number }>;
  unchanged: string[];
}

/** Stable hash over the parts a version bump is keyed on. */
export function definitionContentHash(definition: AutomationDefinition): string {
  const subject = {
    trigger: definition.trigger,
    blocks: definition.blocks,
    inputsSchema: definition.inputsSchema,
    settings: definition.settings,
  };
  return createHash("sha256").update(stableJson(subject)).digest("hex");
}

function stableJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stableJson).join(",")}]`;
  if (typeof value === "object" && value !== null) {
    const keys = Object.keys(value).sort();
    return `{${keys.map((k) => `${JSON.stringify(k)}:${stableJson((value as Record<string, unknown>)[k])}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

/** Resolve seed-time placeholders (the default connection id). */
async function materialize(
  builtin: BuiltinAutomation,
  deps: BuiltinSeedDeps,
): Promise<AutomationDefinition> {
  const definition = structuredClone(builtin.definition);
  if (
    definition.trigger.kind === "integration" &&
    definition.trigger.connectionId === DEFAULT_CONNECTION_PLACEHOLDER
  ) {
    const provider = definition.trigger.provider;
    const connection = await deps.connections.ensureDefault(
      provider,
      `${provider.charAt(0).toUpperCase()}${provider.slice(1)} (default)`,
    );
    definition.trigger.connectionId = connection.id;
  }
  return definition;
}

/** The review enrollment lift: each legacy review_enrollment row becomes an
 * entry in the `repos` map input. Only on first seed; the input is the
 * org's afterwards. */
async function liftEnrollments(
  deps: BuiltinSeedDeps,
): Promise<Record<string, { mode: "auto" | "on_request"; autofix: boolean }>> {
  if (!deps.enrollments) return {};
  const repos: Record<string, { mode: "auto" | "on_request"; autofix: boolean }> = {};
  for (const row of await deps.enrollments.list()) {
    repos[row.repo] = {
      mode: row.triggerMode === "auto" ? "auto" : "on_request",
      autofix: row.autofix !== "off",
    };
  }
  return repos;
}

/** Seed-time guard (ADR 0119 phase 4.3b): a legacy enrollment row (or a
 * default) the schema rejects must not block the built-in from seeding. A
 * bad map ROW drops just that row; any other violation reverts the whole
 * key to its schema default. Every fallback is logged with the violation. */
function sanitizeSeedInputs(
  schema: readonly InputFieldSpec[],
  inputs: Record<string, unknown>,
  logger: { warn(b: Record<string, unknown>, m: string): void },
  builtinKey: string,
): Record<string, unknown> {
  const errors = validateInputValues(schema, inputs);
  if (errors.length === 0) return inputs;
  const out: Record<string, unknown> = { ...inputs };
  for (const error of errors) {
    const spec = schema.find((f) => f.key === error.key);
    const current = out[error.key];
    const rowKey = error.path?.split(".")[0];
    if (
      spec?.type === "map" &&
      rowKey !== undefined &&
      typeof current === "object" &&
      current !== null &&
      !Array.isArray(current) &&
      rowKey in (current as Record<string, unknown>)
    ) {
      const { [rowKey]: _dropped, ...rest } = current as Record<string, unknown>;
      out[error.key] = rest;
    } else {
      out[error.key] = spec?.default;
    }
    logger.warn(
      { builtinKey, key: error.key, path: error.path, message: error.message },
      "built-in seed: input value rejected by its schema; falling back to the default",
    );
  }
  return out;
}

async function seedOne(
  builtin: BuiltinAutomation,
  deps: BuiltinSeedDeps,
  result: BuiltinSeedResult,
): Promise<void> {
  const logger = deps.log ?? log;
  const definition = await materialize(builtin, deps);
  const hash = definitionContentHash(definition);
  const existing = await deps.store.getByBuiltinKey(builtin.key);

  if (!existing) {
    const assembled = await builtin.defaultInputs();
    if (builtin.key === PR_REVIEW_BUILTIN_KEY) {
      assembled["repos"] = await liftEnrollments(deps);
    }
    const inputs = sanitizeSeedInputs(definition.inputsSchema, assembled, logger, builtin.key);
    try {
      await deps.store.create(
        {
          name: builtin.name,
          description: builtin.description,
          enabled: false,
          definition,
          nextFireAt: null,
          kind: "builtin",
          builtinKey: builtin.key,
          inputs,
        },
        null,
      );
    } catch (error) {
      // Another replica won the race on the unique builtin_key index.
      if (isUniqueViolation(error)) return;
      throw error;
    }
    result.created.push(builtin.key);
    logger.info({ key: builtin.key, repos: Object.keys((inputs["repos"] as object) ?? {}).length }, "built-in automation seeded (disabled)");
    return;
  }

  const current = existing.version;
  const stored: AutomationDefinition = {
    engine: 1,
    trigger: current.trigger,
    blocks: current.blocks,
    inputsSchema: current.inputsSchema,
    settings: current.settings,
  };
  if (definitionContentHash(stored) === hash) {
    result.unchanged.push(builtin.key);
    return;
  }

  await bumpVersion(existing, stored, definition, builtin, deps);
  result.bumped.push({ key: builtin.key, from: existing.currentVersion, to: existing.currentVersion + 1 });
  logger.info(
    { key: builtin.key, from: existing.currentVersion, to: existing.currentVersion + 1 },
    "built-in automation bumped to the shipped definition",
  );
}

async function bumpVersion(
  existing: AutomationRow,
  stored: AutomationDefinition,
  shipped: AutomationDefinition,
  builtin: BuiltinAutomation,
  deps: BuiltinSeedDeps,
): Promise<void> {
  // 1. Inputs: add defaults for NEW keys; never overwrite an org value. The
  //    defaults we ship are validated like everything else.
  const defaults = sanitizeSeedInputs(
    shipped.inputsSchema,
    await builtin.defaultInputs(),
    deps.log ?? log,
    builtin.key,
  );
  const mergedInputs: Record<string, unknown> = { ...existing.inputs };
  for (const field of shipped.inputsSchema) {
    if (!(field.key in mergedInputs) && field.key in defaults) {
      mergedInputs[field.key] = defaults[field.key];
    }
  }

  // 2. Block overrides: three-way per block over the shipped-old/shipped-new
  //    configs; an override for a block the new version dropped is discarded.
  //    Same target set as the run-time merge (graph + finalize hooks).
  const oldById = new Map<string, BlockDef>();
  for (const block of overrideTargets(stored)) oldById.set(block.id, block);
  const newById = new Map<string, BlockDef>();
  for (const block of overrideTargets(shipped)) newById.set(block.id, block);
  const reconciled: BlockOverrides = {};
  for (const [blockId, override] of Object.entries(existing.blockOverrides)) {
    const oldBlock = oldById.get(blockId);
    const newBlock = newById.get(blockId);
    if (!newBlock) continue;
    const tunable = new Set(newBlock.tunable ?? []);
    const kept = mergeBuiltinOverrides(oldBlock?.config ?? {}, newBlock.config, override);
    const allowed: Record<string, unknown> = {};
    for (const [field, value] of Object.entries(kept)) {
      if (tunable.has(field)) allowed[field] = value;
    }
    if (Object.keys(allowed).length > 0) reconciled[blockId] = allowed;
  }

  await deps.store.saveVersion(existing.id, shipped, null);
  await deps.store.setInputs(existing.id, mergedInputs);
  await deps.store.setBlockOverrides(existing.id, reconciled);
}

/** Seed every registered built-in. Safe to call on every boot. */
export async function seedBuiltinAutomations(deps: BuiltinSeedDeps): Promise<BuiltinSeedResult> {
  const result: BuiltinSeedResult = { created: [], bumped: [], unchanged: [] };
  const builtins = deps.builtins ?? listBuiltinAutomations();
  for (const builtin of builtins) {
    await seedOne(builtin, deps, result);
  }
  return result;
}

let registered = false;
/** Register the shipped built-ins with the engine's registry (once). */
export function registerShippedBuiltins(): void {
  if (registered) return;
  registered = true;
  registerBuiltinAutomation(PR_REVIEW_BUILTIN);
}

/** Production wiring; fire-and-forget at boot next to the reviewer-profile seed. */
export function productionBuiltinSeedDeps(): BuiltinSeedDeps {
  return {
    store: makeAutomationStore(),
    connections: makeIntegrationConnectionStore(),
    enrollments: makeEnrollmentStore(),
  };
}
