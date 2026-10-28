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
import { getBlock } from "./blocks/registry.ts";

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
  /** The human title, rendered once at instance open in the same scope as
   * the key (e.g. the Slack mention's text, `| strip_mentions | truncate:
   * 80`). The key stays the identity; this is what the UI shows. Empty =
   * the UI falls back to the key. */
  labelTemplate: z.string().min(1).optional(),
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
// The agent: an automation is an agent with ways in
// ---------------------------------------------------------------------------

/** The step every way in that starts or reaches the agent ends on. Reserved
 * when the automation has an agent: no user block takes this id, so
 * `steps.agent.*` (session_id, started, delivered) is the agent's, always. */
export const AGENT_STEP_ID = "agent";
/** The step a `close` way in ends on. */
export const AGENT_CLOSE_STEP_ID = "agent_close";
/** The executor the agent step runs. Not a user block: the agent is a part
 * of the definition, and validation refuses the type in a block list. */
export const AGENT_BLOCK_TYPE = "run_agent";

/** What one agent is responsible for. `event`: a fresh session per event.
 * `automation`: one standing agent for the whole automation. `key`: one
 * agent per rendered key (a thread, a channel, a pull request, an issue) —
 * the workstream of ADR 0120; the template renders in the admission scope
 * ({trigger, event.raw, inputs}), the one template contract. */
export const agentScopeSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("event") }),
  z.object({ kind: z.literal("automation") }),
  z.object({
    kind: z.literal("key"),
    keyTemplate: z.string().min(1),
    /** The human title of one agent, rendered once when it opens. */
    labelTemplate: z.string().min(1).optional(),
  }),
]);
export type AgentScope = z.infer<typeof agentScopeSchema>;

/** The agent every way in delivers to: the session it runs (the
 * `run_agent` executor's config, minus `reuse`, which the scope decides)
 * and its scope. The field set is the executor's: validation parses the
 * derived step config against the executor's schema, so a new session
 * field reaches the agent with no change here. */
export const agentSpecSchema = z
  .object({
    scope: agentScopeSchema,
    // Lenient here: the executor's schema is the one that speaks, and
    // validation addresses its findings to the agent.
    profileId: z.string(),
    promptTemplate: z.string(),
  })
  .passthrough();
export interface AgentSpec {
  scope: AgentScope;
  profileId: string;
  promptTemplate: string;
  [field: string]: unknown;
}

/** What a way in does to the agent. `start` (the default): open the
 * agent's workstream when none is open, then deliver. `reach`: deliver
 * only to an agent that exists (by handle or by key); otherwise the event
 * drops, audited. `close`: end the agent's workstream. `none`: run the
 * blocks and nothing more. */
export const entrypointRoleSchema = z.enum(["start", "reach", "close", "none"]);
export type EntrypointRole = z.infer<typeof entrypointRoleSchema>;

/** The `run_agent` step config the agent spec stands for. */
export function agentStepConfig(agent: AgentSpec): Record<string, unknown> {
  const { scope, ...config } = agent;
  return { ...config, reuse: scope.kind === "event" ? "never" : "workstream" };
}

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
  role: entrypointRoleSchema.optional(),
  blocks: z.array(blockDefSchema),
});

export interface AutomationEntrypoint {
  id: string;
  trigger: TriggerSpec;
  /** With an agent: what this way in does to it (absent = `start`). */
  role?: EntrypointRole;
  blocks: BlockDef[];
}

export const definitionSchema = z.object({
  engine: z.literal(ENGINE_VERSION),
  trigger: triggerSpecSchema,
  /** The main way in's role (see `EntrypointRole`). */
  role: entrypointRoleSchema.optional(),
  blocks: z.array(blockDefSchema),
  /** Additional named ways into the SAME automation (shared inputs,
   * settings, and automation_state). Each run enters through exactly one
   * entrypoint and walks only its blocks. Absent = the classic
   * single-entrypoint automation, byte-identical to the pre-D9 shape. */
  entrypoints: z.array(entrypointSchema).max(MAX_ENTRYPOINTS).optional(),
  /** The agent the ways in deliver to. Present = an agent automation: every
   * way in ends on the agent step unless its role says otherwise, and
   * `settings.instance` is derived from the scope (never stored). Absent =
   * a block automation (the advanced shape): the blocks are the whole run. */
  agent: agentSpecSchema.optional(),
  inputsSchema: z.array(inputFieldSchema),
  settings: settingsSchema,
});

export interface AutomationDefinition {
  engine: typeof ENGINE_VERSION;
  trigger: TriggerSpec;
  role?: EntrypointRole;
  blocks: BlockDef[];
  entrypoints?: AutomationEntrypoint[];
  agent?: AgentSpec;
  inputsSchema: InputFieldSpec[];
  settings: AutomationSettings;
}

/** Every way in as written, main first: the blocks the author declared,
 * without the agent step. The editor's view, and the reserved-id check's. */
export function declaredEntrypointsOf(definition: AutomationDefinition): AutomationEntrypoint[] {
  return [
    {
      id: MAIN_ENTRYPOINT_ID,
      trigger: definition.trigger,
      ...(definition.role !== undefined ? { role: definition.role } : {}),
      blocks: definition.blocks,
    },
    ...(definition.entrypoints ?? []),
  ];
}

export function roleOf(entrypoint: Pick<AutomationEntrypoint, "role">): EntrypointRole {
  return entrypoint.role ?? "start";
}

/** The step a way in ends on because of its role. */
function agentStepsOf(agent: AgentSpec, entrypoint: AutomationEntrypoint): BlockDef[] {
  switch (roleOf(entrypoint)) {
    case "start":
    case "reach":
      return [{ id: AGENT_STEP_ID, type: AGENT_BLOCK_TYPE, config: agentStepConfig(agent) }];
    case "close":
      return [
        {
          id: AGENT_CLOSE_STEP_ID,
          type: "instance_close",
          config: { reason: `closed by the "${entrypoint.id}" way in` },
        },
      ];
    case "none":
      return [];
  }
}

/** Every way in, main first, as the run walks it: with an agent, each way
 * in ends on the step its role names. The one view dispatch, validation,
 * and the interpreter share. */
export function entrypointsOf(definition: AutomationDefinition): AutomationEntrypoint[] {
  const declared = declaredEntrypointsOf(definition);
  const agent = definition.agent;
  if (agent === undefined) return declared;
  return declared.map((entrypoint) => ({
    ...entrypoint,
    blocks: [...entrypoint.blocks, ...agentStepsOf(agent, entrypoint)],
  }));
}

/** The workstream settings the run system reads. A block automation
 * declares them; an agent automation derives them from the agent's scope
 * and the ways in's roles (`reach` and `close` only find an open agent). */
export function instanceSettingsOf(definition: AutomationDefinition): InstanceSettings | undefined {
  const agent = definition.agent;
  if (agent === undefined) return definition.settings.instance;
  const scope = agent.scope;
  if (scope.kind === "event") return undefined;
  const entrypoints: Record<string, { admit: InstanceAdmitPolicy }> = {};
  for (const entrypoint of declaredEntrypointsOf(definition)) {
    const role = roleOf(entrypoint);
    if (role === "reach" || role === "close") entrypoints[entrypoint.id] = { admit: "require" };
  }
  return {
    keyTemplate: scope.kind === "automation" ? AGENT_STEP_ID : scope.keyTemplate,
    ...(scope.kind === "key" && scope.labelTemplate !== undefined
      ? { labelTemplate: scope.labelTemplate }
      : {}),
    ...(Object.keys(entrypoints).length > 0 ? { entrypoints } : {}),
  };
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
 * config schemas, nested condition groups, and every
 * embedded Liquid template. Throws `DefinitionError` with a block+field
 * address the editor can route. */
export function validateDefinition(raw: unknown): AutomationDefinition {
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

  const agent = definition.agent;
  if (agent !== undefined) {
    if (definition.settings.instance !== undefined) {
      throw new DefinitionError(
        "__agent__",
        "scope",
        "settings.instance is derived from the agent's scope; remove it",
      );
    }
    const executor = getBlock(AGENT_BLOCK_TYPE);
    if (!executor) throw new DefinitionError("__agent__", "profileId", "the agent executor is not registered");
    const config = saveTimeConfigParse(executor.configSchema, agentStepConfig(agent));
    if (!config.success) {
      const issue = config.error.issues[0];
      throw new DefinitionError(
        "__agent__",
        issue ? issue.path.join(".") : "profileId",
        issue ? issue.message : "invalid agent",
      );
    }
    validateTemplatesIn("__agent__", agentStepConfig(agent), "");
    if (agent.scope.kind === "key") {
      validateTemplatesIn("__agent__", agent.scope.keyTemplate, "scope.keyTemplate");
      if (agent.scope.labelTemplate !== undefined) {
        validateTemplatesIn("__agent__", agent.scope.labelTemplate, "scope.labelTemplate");
      }
    }
  }
  for (const entrypoint of declaredEntrypointsOf(definition)) {
    const where = entrypoint.id === MAIN_ENTRYPOINT_ID ? "__trigger__" : "__entrypoints__";
    if (entrypoint.role !== undefined && agent === undefined) {
      throw new DefinitionError(
        where,
        "role",
        `way in "${entrypoint.id}" has a role but the automation has no agent`,
      );
    }
    const role = roleOf(entrypoint);
    if (agent?.scope.kind === "event" && (role === "reach" || role === "close")) {
      throw new DefinitionError(
        where,
        "role",
        `way in "${entrypoint.id}" cannot ${role} an agent scoped to each event (there is no standing agent to find)`,
      );
    }
    for (const block of walkBlocks(entrypoint.blocks)) {
      if (block.type === AGENT_BLOCK_TYPE) {
        throw new DefinitionError(
          block.id,
          "type",
          `"${AGENT_BLOCK_TYPE}" is not a block; set the automation's agent and the way in's role`,
        );
      }
      if (agent !== undefined && (block.id === AGENT_STEP_ID || block.id === AGENT_CLOSE_STEP_ID)) {
        throw new DefinitionError(block.id, "id", `block id "${block.id}" is the agent step's`);
      }
    }
  }

  const seen = new Set<string>();
  let count = 0;
  const checkBlock = (block: BlockDef, hook: boolean): void => {
    count += 1;
    if (count > MAX_BLOCKS) {
      throw new DefinitionError(null, "blocks", `more than ${MAX_BLOCKS} blocks`);
    }
    if (seen.has(block.id)) {
      throw new DefinitionError(block.id, "id", `duplicate block id "${block.id}"`);
    }
    seen.add(block.id);

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
  }
  // Block ids stay unique across ALL entrypoints (`seen` is shared): a step
  // path addresses one automation-wide namespace. The walk is over the
  // blocks as written: the agent step every way in ends on is one step,
  // validated once above, under its reserved id.
  for (const entrypoint of declaredEntrypointsOf(definition)) {
    for (const block of walkBlocks(entrypoint.blocks)) checkBlock(block, false);
  }
  if (entrypoints.filter((entrypoint) => entrypoint.trigger.kind === "cron").length > 1) {
    throw new DefinitionError(
      "__entrypoints__",
      "trigger",
      "at most one cron trigger per automation (the scheduler tracks one next_fire_at per row)",
    );
  }
  for (const hook of definition.settings.onFinalize ?? []) checkBlock(hook.block, true);

  if (definition.settings.concurrency) {
    validateTemplatesIn("__settings__", definition.settings.concurrency.keyTemplate, "concurrency.keyTemplate");
  }
  const instance = instanceSettingsOf(definition);
  if (instance) {
    validateTemplatesIn("__settings__", instance.keyTemplate, "instance.keyTemplate");
    if (instance.labelTemplate !== undefined) {
      validateTemplatesIn("__settings__", instance.labelTemplate, "instance.labelTemplate");
    }
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
