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

export const settingsSchema = z.object({
  concurrency: z
    .object({
      keyTemplate: z.string().min(1),
      policy: z.enum(["queue", "supersede", "skip", "join"]),
    })
    .optional(),
  runDeadlineSeconds: z.number().int().min(60).max(48 * 3600).optional(),
  endSessionsOnFinish: z.boolean(),
});
export type AutomationSettings = z.infer<typeof settingsSchema>;

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

export interface BlockDef {
  id: string;
  type: string;
  retry?: RetryPolicy;
  /** Type-specific configuration, validated by the block's registered schema. */
  config: Record<string, unknown>;
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
    then: z.array(blockDefSchema).optional(),
    else: z.array(blockDefSchema).optional(),
    body: z.array(blockDefSchema).optional(),
  }),
);

export const definitionSchema = z.object({
  engine: z.literal(ENGINE_VERSION),
  trigger: triggerSpecSchema,
  blocks: z.array(blockDefSchema),
  inputsSchema: z.array(inputFieldSchema),
  settings: settingsSchema,
});

export interface AutomationDefinition {
  engine: typeof ENGINE_VERSION;
  trigger: TriggerSpec;
  blocks: BlockDef[];
  inputsSchema: InputFieldSpec[];
  settings: AutomationSettings;
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

function validateTemplatesIn(blockId: string, value: unknown, field: string): void {
  if (typeof value === "string" && value.includes("${{")) {
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
  for (const block of walkBlocks(definition.blocks)) {
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
    const config = executor.configSchema.safeParse(block.config);
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
    validateTemplatesIn(block.id, block.config, "");
  }

  if (definition.settings.concurrency) {
    validateTemplatesIn("__settings__", definition.settings.concurrency.keyTemplate, "concurrency.keyTemplate");
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
