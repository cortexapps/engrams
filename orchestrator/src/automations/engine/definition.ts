/** Automation definitions as data (ADR 0119 D1).
 *
 * A definition is one immutable `automation_version` row: trigger + ordered
 * block graph + inputs schema + settings. The engine walks the checkpointed
 * snapshot of exactly one version, so validation here is authoring-time
 * defense; the interpreter trusts what it is handed.
 */

import { z } from "zod";

import { validateAutomationTemplate } from "../template.ts";
import { parseFilterGroup, type FilterGroup } from "./conditions.ts";
import { getBlock, isSystemBlockType } from "./blocks/registry.ts";

export const ENGINE_VERSION = 1;
export const MAX_BLOCKS = 64;
export const MAX_LOOP_ITERATIONS = 100;
/** The ceiling every wait block's `deadlineSeconds` schema enforces, and
 * the clamp the interpreter applies to a `$ref`-resolved deadline. */
export const MAX_WAIT_DEADLINE_S = 24 * 3600;

// ---------------------------------------------------------------------------
// Triggers
// ---------------------------------------------------------------------------

const cronTrigger = z.object({
  kind: z.literal("cron"),
  schedule: z.string().min(1),
  timezone: z.string().min(1),
});

const webhookTrigger = z.object({
  kind: z.literal("webhook"),
  registrationId: z.string().min(1),
  events: z.array(z.string().min(1)).min(1),
  filter: z.record(z.string(), z.unknown()).optional(),
});

/** Phase 2 (ADR 0119 D5): integration-owned event triggers. The shape ships
 * now so definitions round-trip; dispatch arrives with the ingress spine. */
const integrationTrigger = z.object({
  kind: z.literal("integration"),
  provider: z.string().min(1),
  connectionId: z.string().min(1),
  eventKeys: z.array(z.string().min(1)).min(1),
  scope: z
    .union([
      z.object({ values: z.array(z.string().min(1)).min(1) }),
      z.object({ fromInput: z.string().min(1) }),
    ])
    .optional(),
  /** Event keys that only CONTINUE an active run (delivered into the
   * concurrency holder's mailbox under policy `join`) and never open one:
   * with no active run for the key, the delivery is dropped at admission.
   * The conversation-shaped built-ins use it — a Slack thread reply belongs
   * to a thread the bot was mentioned in, or to nobody. Must be a subset of
   * `eventKeys`, and requires `settings.concurrency.policy: "join"`. */
  continueOnly: z.array(z.string().min(1)).optional(),
});

const manualTrigger = z.object({ kind: z.literal("manual") });

export const triggerSpecSchema = z.discriminatedUnion("kind", [
  cronTrigger,
  webhookTrigger,
  integrationTrigger,
  manualTrigger,
]);
export type TriggerSpec = z.infer<typeof triggerSpecSchema>;

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

export const inputFieldSchema = z.object({
  key: z.string().regex(/^[a-z][a-z0-9_]*$/),
  label: z.string().min(1).max(120),
  type: z.enum(["string", "number", "boolean", "enum", "secret_ref", "list", "map", "json"]),
  help: z.string().max(500).optional(),
  required: z.boolean().optional(),
  default: z.unknown().optional(),
  /** enum: allowed values. */
  values: z.array(z.string()).optional(),
  /** number: inclusive bounds, enforced on every value write (SetInputs,
   * the seeder, the run snapshot) and mirrored by the web Inputs tab. A
   * block that consumes the input through a `$ref` has its own schema
   * ceiling; bounding the input is what makes a saved value always run. */
  min: z.number().optional(),
  max: z.number().optional(),
  /** string: render a textarea (the web Inputs tab honors it). */
  multiline: z.boolean().optional(),
  /** map: the integration noun that populates the key picker. */
  keyNoun: z.enum(["repository", "channel", "team"]).optional(),
  /** map: field specs for the value object; list: the element type. */
  valueShape: z.record(z.string(), z.unknown()).optional(),
});
export type InputFieldSpec = z.infer<typeof inputFieldSchema>;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

export const retryPolicySchema = z.object({
  attempts: z.number().int().min(1).max(5),
  retryOn: z.enum(["transient", "always", "never"]).optional(),
});
export type RetryPolicy = z.infer<typeof retryPolicySchema>;

/** ADR 0120 instances: how an entrypoint's events reach a workstream.
 * `open` renders the key template and opens (or joins) that instance;
 * `require` joins an existing open instance or DROPS the event (the
 * instance-level continueOnly); `handle_match` routes ONLY through the
 * handle ledger (facet-declared candidate handles), dropping unbound
 * events before a run boots. */
const instanceAdmitSchema = z.enum(["open", "require", "handle_match"]);

const instanceSettingsSchema = z.object({
  /** The identity template (e.g. `project-${{ inputs.linear_project_id }}`),
   * rendered in the concurrency-key scope ({trigger, event.raw, inputs}) —
   * one template contract for authors. */
  keyTemplate: z.string().min(1),
  /** Input-snapshot templates rendered at instance open: field key →
   * template over the admitting event. Unlisted fields snapshot the
   * automation row's value (which demotes to "defaults for new
   * instances"). */
  inputs: z.record(z.string().regex(/^[a-z][a-z0-9_]*$/), z.string().min(1)).optional(),
  /** Per-entrypoint admission override; absent = `open`. */
  entrypoints: z
    .record(z.string().min(1), z.object({ admit: instanceAdmitSchema }))
    .optional(),
});
export type InstanceSettings = z.infer<typeof instanceSettingsSchema>;
export type InstanceAdmitPolicy = z.infer<typeof instanceAdmitSchema>;

const settingsBaseSchema = z.object({
  /** ADR 0120: present = the automation is instanced (a "workstream" per
   * rendered key). Absence is byte-identical pre-instance behavior. */
  instance: instanceSettingsSchema.optional(),
  concurrency: z
    .object({
      keyTemplate: z.string().min(1),
      policy: z.enum(["queue", "supersede", "skip", "join"]),
    })
    .optional(),
  runDeadlineSeconds: z.number().int().min(60).max(48 * 3600).optional(),
  endSessionsOnFinish: z.boolean(),
});

/** Terminal statuses a finalize hook can fire on. Mirrors RunTerminalStatus
 * (interpreter.ts); kept literal here so the definition module stays
 * import-free of the interpreter. */
export const FINALIZE_HOOK_STATUSES = [
  "completed",
  "filtered",
  "failed",
  "superseded",
  "halted",
  "deadline",
] as const;
export type FinalizeHookStatus = (typeof FINALIZE_HOOK_STATUSES)[number];
export const MAX_FINALIZE_HOOKS = 8;

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

export interface BlockDef {
  id: string;
  type: string;
  retry?: RetryPolicy;
  /** Type-specific configuration, validated by the block's registered schema. */
  config: Record<string, unknown>;
  /** Config field names a user may override per automation without editing
   * the graph (built-ins: structure locked, properties editable). Absent or
   * empty = nothing tunable. Overrides live in `automation.block_overrides`
   * and are merged over `config` at snapshot time. */
  tunable?: string[];
  /** branch */
  then?: BlockDef[];
  else?: BlockDef[];
  /** loop */
  body?: BlockDef[];
}

const blockDefSchema: z.ZodType<BlockDef> = z.lazy(() =>
  z.object({
    id: z.string().regex(/^[a-z][a-z0-9_]*$/),
    type: z.string().min(1),
    retry: retryPolicySchema.optional(),
    config: z.record(z.string(), z.unknown()),
    tunable: z.array(z.string().regex(/^[a-zA-Z][a-zA-Z0-9_]*$/)).max(64).optional(),
    then: z.array(blockDefSchema).optional(),
    else: z.array(blockDefSchema).optional(),
    body: z.array(blockDefSchema).optional(),
  }),
);

/** `{ "$ref": "<safe.path>" }` — a run-time value reference (ADR 0119
 * phase 4.3): the interpreter replaces it with the JSON value at that scope
 * path before the block executes. Liquid yields strings only; structured
 * outputs of earlier blocks pass this way. */
export function isValueRef(value: unknown): value is { $ref: string } {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const keys = Object.keys(value);
  return keys.length === 1 && keys[0] === "$ref" && typeof (value as { $ref: unknown }).$ref === "string";
}

/** A config with every RUN-TIME-VALUED field removed, for save-time schema
 * checks: `$ref` objects and templated strings both take their final shape
 * only when the run resolves them (a `${{ steps.open.head_sha }}` cannot
 * satisfy a SHA regex at save time). Templates are still parse-validated by
 * validateTemplatesIn, and the interpreter re-runs the full schema on the
 * resolved config inside the block's step. Nested objects are walked, and a
 * container emptied by stripping is dropped whole — a half-known object
 * (`session: {}` after its `template` stripped) satisfies no schema and
 * would fail as noise. */
export function withoutValueRefs(config: Record<string, unknown>): Record<string, unknown> {
  return stripRunTimeValues(config).config;
}

function stripRunTimeValues(config: Record<string, unknown>): {
  config: Record<string, unknown>;
  stripped: boolean;
} {
  let stripped = false;
  const strip = (value: unknown): unknown => {
    if (isValueRef(value)) {
      stripped = true;
      return undefined;
    }
    if (typeof value === "string" && value.includes("${{")) {
      stripped = true;
      return undefined;
    }
    if (Array.isArray(value)) {
      const items = value.map(strip).filter((v) => v !== undefined);
      // An array whose every element was templated is unknowable; drop it.
      return items.length === 0 && value.length > 0 ? undefined : items;
    }
    if (typeof value === "object" && value !== null) {
      const out: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(value)) {
        const s = strip(v);
        if (s !== undefined) out[k] = s;
      }
      // Same rule as arrays: an object emptied by stripping is unknowable.
      return Object.keys(out).length === 0 && Object.keys(value).length > 0 ? undefined : out;
    }
    return value;
  };
  const result = strip(config);
  return {
    config: (typeof result === "object" && result !== null ? result : {}) as Record<
      string,
      unknown
    >,
    stripped,
  };
}

/** Save-time config check: strip run-time values, then validate against the
 * schema — relaxed to per-key optional iff ANYTHING was stripped, so a key
 * whose value (or nested value: a templated `session.template` strips its
 * whole ref) is run-time-valued is not a "required" failure, while any key
 * that IS present still validates in full. One function on purpose: the
 * strip and the relaxation deciding differently is exactly the bug that
 * made every templated session ref fail SaveVersion with "Invalid input".
 * Block schemas are z.object by convention; anything else validates
 * as-is. */
function saveTimeConfigParse(
  schema: z.ZodType,
  config: Record<string, unknown>,
): z.ZodSafeParseResult<unknown> {
  const { config: strippedConfig, stripped } = stripRunTimeValues(config);
  const effective =
    stripped && schema instanceof z.ZodObject ? schema.partial() : schema;
  return effective.safeParse(strippedConfig);
}

/** Block overrides as stored on the automation row. */
export type BlockOverrides = Record<string, Record<string, unknown>>;

export class BlockOverrideError extends Error {
  constructor(
    readonly blockId: string,
    readonly field: string,
    message: string,
  ) {
    super(message);
    this.name = "BlockOverrideError";
  }
}

function* walkBlockTree(blocks: BlockDef[]): Generator<BlockDef> {
  for (const block of blocks) {
    yield block;
    if (block.then) yield* walkBlockTree(block.then);
    if (block.else) yield* walkBlockTree(block.else);
    if (block.body) yield* walkBlockTree(block.body);
  }
}

/** Every block an override can target: the graph (depth-first) AND the
 * finalize-hook blocks. The one list both the run-time merge and the
 * built-in seeder's version-bump reconciliation walk — a hook block with a
 * `tunable` field is editable exactly like a graph block. */
export function* overrideTargets(definition: AutomationDefinition): Generator<BlockDef> {
  yield* walkBlockTree(definition.blocks);
  for (const entrypoint of definition.entrypoints ?? []) yield* walkBlockTree(entrypoint.blocks);
  for (const hook of definition.settings.onFinalize ?? []) yield hook.block;
}

/** Validate overrides against a definition: every block id must exist, every
 * field must be listed in that block's `tunable`, and the merged config must
 * still satisfy the block's registered schema. Returns the merged definition
 * (the engine walks the merge; it never sees overrides). */
export function applyBlockOverrides(
  definition: AutomationDefinition,
  overrides: BlockOverrides,
): AutomationDefinition {
  const byId = new Map<string, BlockDef>();
  for (const block of overrideTargets(definition)) byId.set(block.id, block);

  for (const [blockId, fields] of Object.entries(overrides)) {
    const block = byId.get(blockId);
    if (!block) {
      throw new BlockOverrideError(blockId, "", `block "${blockId}" is not in this automation`);
    }
    const tunable = new Set(block.tunable ?? []);
    for (const field of Object.keys(fields)) {
      if (!tunable.has(field)) {
        throw new BlockOverrideError(
          blockId,
          field,
          `field "${field}" of block "${blockId}" is not tunable`,
        );
      }
    }
  }

  const merge = (blocks: BlockDef[]): BlockDef[] =>
    blocks.map((block) => {
      const fields = overrides[block.id];
      const merged: BlockDef = {
        ...block,
        config: fields ? { ...block.config, ...fields } : block.config,
        ...(block.then ? { then: merge(block.then) } : {}),
        ...(block.else ? { else: merge(block.else) } : {}),
        ...(block.body ? { body: merge(block.body) } : {}),
      };
      if (fields) {
        const executor = getBlock(block.type);
        const parsed = executor
          ? saveTimeConfigParse(executor.configSchema, merged.config)
          : undefined;
        if (parsed && !parsed.success) {
          const issue = parsed.error.issues[0];
          throw new BlockOverrideError(
            block.id,
            issue ? issue.path.join(".") : "config",
            issue ? issue.message : "override produces an invalid block config",
          );
        }
      }
      return merged;
    });

  const onFinalize = definition.settings.onFinalize?.map((hook) => ({
    ...hook,
    block: merge([hook.block])[0]!,
  }));
  return {
    ...definition,
    blocks: merge(definition.blocks),
    ...(definition.entrypoints
      ? {
          entrypoints: definition.entrypoints.map((entrypoint) => ({
            ...entrypoint,
            blocks: merge(entrypoint.blocks),
          })),
        }
      : {}),
    settings: onFinalize ? { ...definition.settings, onFinalize } : definition.settings,
  };
}

/** A finalize-time hook (ADR 0119, contract 2): a block that runs inside the
 * finalize step when the run ends in one of `when`. Hooks observe the
 * terminal status (`run.status`, `run.error` in scope) and may post, clean up,
 * or record — they can never change the outcome, and they never wait. */
export interface FinalizeHook {
  when: FinalizeHookStatus[];
  block: BlockDef;
}

const finalizeHookSchema: z.ZodType<FinalizeHook> = z.object({
  when: z.array(z.enum(FINALIZE_HOOK_STATUSES)).min(1),
  block: blockDefSchema,
});

export const settingsSchema = settingsBaseSchema.extend({
  onFinalize: z.array(finalizeHookSchema).max(MAX_FINALIZE_HOOKS).optional(),
});
export type AutomationSettings = z.infer<typeof settingsSchema>;

// ---------------------------------------------------------------------------
// Entrypoints (ADR 0119 D9)
// ---------------------------------------------------------------------------

/** The implicit entrypoint every automation has: the top-level
 * `trigger` + `blocks`. Extra entrypoints are named and never "main". */
export const MAIN_ENTRYPOINT_ID = "main";
export const MAX_ENTRYPOINTS = 8;

/** Extra entrypoints take integration, cron, or manual triggers. The legacy
 * `webhook` trigger stays main-only: its alias-resolution path is
 * registration-scoped and retires with ADR 0119 phase 2. */
const entrypointTriggerSchema = z.discriminatedUnion("kind", [
  cronTrigger,
  integrationTrigger,
  manualTrigger,
]);

export const entrypointSchema = z.object({
  id: z
    .string()
    .regex(/^[a-z][a-z0-9_]*$/)
    .max(64)
    .refine((id) => id !== MAIN_ENTRYPOINT_ID, `"${MAIN_ENTRYPOINT_ID}" names the implicit top-level entrypoint`),
  trigger: entrypointTriggerSchema,
  blocks: z.array(blockDefSchema),
});

export interface AutomationEntrypoint {
  id: string;
  trigger: TriggerSpec;
  blocks: BlockDef[];
}

export const definitionSchema = z.object({
  engine: z.literal(ENGINE_VERSION),
  trigger: triggerSpecSchema,
  blocks: z.array(blockDefSchema),
  /** Additional named ways into the SAME automation (shared inputs,
   * settings, and automation_state). Each run enters through exactly one
   * entrypoint and walks only its blocks. Absent = the classic
   * single-entrypoint automation, byte-identical to the pre-D9 shape. */
  entrypoints: z.array(entrypointSchema).max(MAX_ENTRYPOINTS).optional(),
  inputsSchema: z.array(inputFieldSchema),
  settings: settingsSchema,
});

export interface AutomationDefinition {
  engine: typeof ENGINE_VERSION;
  trigger: TriggerSpec;
  blocks: BlockDef[];
  entrypoints?: AutomationEntrypoint[];
  inputsSchema: InputFieldSpec[];
  settings: AutomationSettings;
}

/** Every way in, main first. The one view dispatch, validation, and the
 * interpreter share. */
export function entrypointsOf(definition: AutomationDefinition): AutomationEntrypoint[] {
  return [
    { id: MAIN_ENTRYPOINT_ID, trigger: definition.trigger, blocks: definition.blocks },
    ...(definition.entrypoints ?? []),
  ];
}

export function entrypointOf(
  definition: AutomationDefinition,
  id: string,
): AutomationEntrypoint | null {
  return entrypointsOf(definition).find((entrypoint) => entrypoint.id === id) ?? null;
}

/** The automation's single cron entrypoint, if any. Validation caps cron
 * triggers at one per automation because the scheduler tracks ONE
 * `next_fire_at` per automation row. */
export function cronEntrypointOf(definition: AutomationDefinition): AutomationEntrypoint | null {
  return entrypointsOf(definition).find((entrypoint) => entrypoint.trigger.kind === "cron") ?? null;
}

// ---------------------------------------------------------------------------
// Session references shared by session-facing blocks
// ---------------------------------------------------------------------------

export const sessionRefSchema = z.union([
  z.object({ blockId: z.string().min(1) }),
  z.object({ template: z.string().min(1) }),
]);
export type SessionRef = z.infer<typeof sessionRefSchema>;

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

export class DefinitionError extends Error {
  constructor(
    readonly blockId: string | null,
    readonly field: string,
    message: string,
  ) {
    super(message);
    this.name = "DefinitionError";
  }
}

export interface ValidateDefinitionOptions {
  kind: "user" | "builtin";
}

function* walkBlocks(blocks: BlockDef[]): Generator<BlockDef> {
  for (const block of blocks) {
    yield block;
    if (block.then) yield* walkBlocks(block.then);
    if (block.else) yield* walkBlocks(block.else);
    if (block.body) yield* walkBlocks(block.body);
  }
}

/** Top-level config fields the interpreter's config walk leaves UNRENDERED
 * (see resolveBlockConfig): a code block's `source` is JS, condition
 * groups / wait specs are data. Template validation skips their strings for
 * the same reason — a `{{` in JS source or in a compared value is content,
 * not a template. `session` is deliberately NOT here: the walk skips it,
 * but the session-facing BLOCK renders `session.template` itself, so it
 * carries the full template contract. */
export const NON_TEMPLATE_CONFIG_FIELDS: ReadonlySet<string> = new Set([
  "source",
  "conditions",
  "until",
  "waitFor",
]);

function validateTemplatesIn(blockId: string, value: unknown, field: string): void {
  if (typeof value === "string") {
    // The one Liquid delimiter is `${{ … }}`. A plain `{{ … }}` in a
    // rendered field never renders — it flows through as a literal — which
    // is virtually always a delimiter mistake, and a silent one (the
    // drafting-agent incident: a session ref of "{{ steps.x.value }}"
    // validated clean and then adopted the literal string at runtime).
    // Refuse it at save with the fix in the message.
    // Remove well-formed template regions first, so a template that OUTPUTS
    // braces (${{ '{{' }}) is not misread; an unclosed ${{ falls through to
    // the Liquid parser's own error below.
    const outsideTemplates = value.replace(/\$\{\{[\s\S]*?\}\}/g, "");
    if (!outsideTemplates.includes("${{") && outsideTemplates.includes("{{")) {
      throw new DefinitionError(
        blockId,
        field,
        "plain \"{{ … }}\" is never rendered — automation templates use ${{ … }} delimiters. " +
          "For a literal \"{{\" in output, write ${{ '{{' }}.",
      );
    }
    if (!value.includes("${{")) return;
    try {
      validateAutomationTemplate(value);
    } catch (error) {
      throw new DefinitionError(
        blockId,
        field,
        error instanceof Error ? error.message : String(error),
      );
    }
    return;
  }
  if (Array.isArray(value)) {
    value.forEach((item, i) => validateTemplatesIn(blockId, item, `${field}[${i}]`));
    return;
  }
  if (typeof value === "object" && value !== null) {
    for (const [key, child] of Object.entries(value)) {
      // The skip applies at the BLOCK-CONFIG top level only, mirroring the
      // interpreter's walk (field === "" = the config object itself).
      if (field === "" && NON_TEMPLATE_CONFIG_FIELDS.has(key)) continue;
      validateTemplatesIn(blockId, child, field === "" ? key : `${field}.${key}`);
    }
  }
}

/** Authoring-time validation: shape, unique ids, block-type existence and
 * config schemas, system-block gating, nested condition groups, and every
 * embedded Liquid template. Throws `DefinitionError` with a block+field
 * address the editor can route. */
export function validateDefinition(
  raw: unknown,
  options: ValidateDefinitionOptions,
): AutomationDefinition {
  const parsed = definitionSchema.safeParse(raw);
  if (!parsed.success) {
    const issue = parsed.error.issues[0];
    throw new DefinitionError(
      null,
      issue ? issue.path.join(".") : "definition",
      issue ? issue.message : "invalid definition",
    );
  }
  const definition = parsed.data as AutomationDefinition;

  const seen = new Set<string>();
  let count = 0;
  /** Contract 3: one message handler per RUN — a run walks one entrypoint,
   * so the installer list resets per entrypoint. Several blocks may carry it
   * as long as they are the SAME type — a later one (a loop body re-pointing
   * the Slack relay at a new turn) is a re-point of the installed handler,
   * never a second install (the interpreter enforces the same executor). */
  let installers: Array<{ id: string; type: string }> = [];
  const checkBlock = (block: BlockDef, hook: boolean): void => {
    count += 1;
    if (count > MAX_BLOCKS) {
      throw new DefinitionError(null, "blocks", `more than ${MAX_BLOCKS} blocks`);
    }
    if (seen.has(block.id)) {
      throw new DefinitionError(block.id, "id", `duplicate block id "${block.id}"`);
    }
    seen.add(block.id);

    if (isSystemBlockType(block.type) && options.kind !== "builtin") {
      throw new DefinitionError(
        block.id,
        "type",
        `block type "${block.type}" is reserved for built-in automations`,
      );
    }
    const executor = getBlock(block.type);
    if (!executor) {
      throw new DefinitionError(block.id, "type", `unknown block type "${block.type}"`);
    }
    // Run-time-valued fields (`$ref` objects, templated strings) take their
    // final shape only when the run resolves them; validate the statically
    // known remainder now — leniently on the keys that were stripped — and
    // the fully resolved config again in the interpreter step.
    const config = saveTimeConfigParse(executor.configSchema, block.config);
    if (!config.success) {
      const issue = config.error.issues[0];
      throw new DefinitionError(
        block.id,
        issue ? issue.path.join(".") : "config",
        issue ? issue.message : "invalid block config",
      );
    }
    if (block.type === "branch" && !block.then) {
      throw new DefinitionError(block.id, "then", "branch needs a then list");
    }
    if (block.type === "loop" && !block.body) {
      throw new DefinitionError(block.id, "body", "loop needs a body");
    }
    if (block.type !== "branch" && (block.then || block.else)) {
      throw new DefinitionError(block.id, "then", `only branch blocks nest then/else`);
    }
    if (block.type !== "loop" && block.body) {
      throw new DefinitionError(block.id, "body", `only loop blocks nest a body`);
    }
    if (executor.onMessage !== undefined) {
      // Contract 3: an installed message handler rides the run's single recv
      // loop, so exactly one may exist, and a finalize hook (which runs after
      // the loop is over) can never install one.
      if (hook) {
        throw new DefinitionError(
          block.id,
          "type",
          `block type "${block.type}" installs a message handler; not allowed in a finalize hook`,
        );
      }
      const first = installers[0];
      if (first !== undefined && first.type !== block.type) {
        throw new DefinitionError(
          block.id,
          "type",
          `only one message-handler type per automation (already: "${first.id}" of type "${first.type}")`,
        );
      }
      installers.push({ id: block.id, type: block.type });
    }
    if (hook) {
      // A finalize hook runs inside the finalize step: nothing may park on
      // the mailbox there, and control flow has no graph to branch into.
      if (executor.wait !== undefined || block.type === "wait_event") {
        throw new DefinitionError(
          block.id,
          "type",
          `block type "${block.type}" waits; a finalize hook cannot wait`,
        );
      }
      if (block.type === "branch" || block.type === "loop" || block.type === "filter") {
        throw new DefinitionError(
          block.id,
          "type",
          `control block "${block.type}" is not allowed in a finalize hook`,
        );
      }
    }
    validateTemplatesIn(block.id, block.config, "");
  };

  const entrypoints = entrypointsOf(definition);
  const entrypointIds = new Set<string>();
  for (const entrypoint of entrypoints) {
    if (entrypointIds.has(entrypoint.id)) {
      throw new DefinitionError(
        "__entrypoints__",
        "id",
        `duplicate entrypoint id "${entrypoint.id}"`,
      );
    }
    entrypointIds.add(entrypoint.id);
    // Block ids stay unique across ALL entrypoints (`seen` is shared): a
    // step path addresses one automation-wide namespace.
    installers = [];
    for (const block of walkBlocks(entrypoint.blocks)) checkBlock(block, false);
  }
  if (entrypoints.filter((entrypoint) => entrypoint.trigger.kind === "cron").length > 1) {
    throw new DefinitionError(
      "__entrypoints__",
      "trigger",
      "at most one cron trigger per automation (the scheduler tracks one next_fire_at per row)",
    );
  }
  installers = [];
  for (const hook of definition.settings.onFinalize ?? []) checkBlock(hook.block, true);

  if (definition.settings.concurrency) {
    validateTemplatesIn("__settings__", definition.settings.concurrency.keyTemplate, "concurrency.keyTemplate");
  }
  const instance = definition.settings.instance;
  if (instance) {
    validateTemplatesIn("__settings__", instance.keyTemplate, "instance.keyTemplate");
    const inputKeys = new Set(definition.inputsSchema.map((field) => field.key));
    for (const [key, template] of Object.entries(instance.inputs ?? {})) {
      if (!inputKeys.has(key)) {
        throw new DefinitionError(
          "__settings__",
          "instance.inputs",
          `instance input "${key}" is not an inputsSchema field`,
        );
      }
      validateTemplatesIn("__settings__", template, `instance.inputs.${key}`);
    }
    for (const [entrypointId, policy] of Object.entries(instance.entrypoints ?? {})) {
      const entrypoint = entrypoints.find((candidate) => candidate.id === entrypointId);
      if (!entrypoint) {
        throw new DefinitionError(
          "__settings__",
          "instance.entrypoints",
          `instance admission names unknown entrypoint "${entrypointId}"`,
        );
      }
      if (
        policy.admit === "handle_match" &&
        entrypoint.trigger.kind !== "integration" &&
        entrypoint.trigger.kind !== "webhook"
      ) {
        throw new DefinitionError(
          "__settings__",
          "instance.entrypoints",
          `admit "handle_match" needs a delivery-bearing trigger on "${entrypointId}" (a ${entrypoint.trigger.kind} event carries no candidate handles)`,
        );
      }
    }
  }
  for (const entrypoint of entrypoints) {
    const trigger = entrypoint.trigger;
    if (trigger.kind !== "integration" || trigger.continueOnly === undefined) continue;
    for (const key of trigger.continueOnly) {
      if (!trigger.eventKeys.includes(key)) {
        throw new DefinitionError(
          "__trigger__",
          "continueOnly",
          `continueOnly event "${key}" is not one of the trigger's eventKeys`,
        );
      }
    }
    if (definition.settings.concurrency?.policy !== "join") {
      throw new DefinitionError(
        "__trigger__",
        "continueOnly",
        "continueOnly needs settings.concurrency.policy \"join\" (a continue-only event joins the active run's mailbox)",
      );
    }
  }

  return definition;
}

/** Parse the condition group a control block carries; `DefinitionError` on
 * bad shape so save-time surfaces the field. */
export function parseBlockConditions(blockId: string, raw: unknown): FilterGroup {
  try {
    return parseFilterGroup(raw);
  } catch (error) {
    throw new DefinitionError(
      blockId,
      "conditions",
      error instanceof Error ? error.message : String(error),
    );
  }
}
