/** Injected tools for the automation-drafting session (Builder v2).
 *
 * Gated by task type "automation_draft"; authorized by the binding — a tool
 * call may only touch the automation whose `draft_session_id` is its own
 * session. Mutations follow the soft-refusal contract: validation failures
 * and version conflicts return `applied: false` with addressed errors, never
 * a thrown error, so the model self-corrects.
 *
 * MCP constraint (see tools/builtin.ts): every input schema is one flat
 * `type: "object"` — a single `anyOf` at the top level makes the claude CLI
 * reject the WHOLE tools/list.
 */

import { z } from "zod";

import { definitionOf, effectiveDefinition, SaveVersionConflictError } from "../db/automations.ts";
import type { AutomationRow, AutomationStore } from "../db/automations.ts";
import type { ProfileStore } from "../db/profiles.ts";
import {
  DefinitionError,
  entrypointOf,
  MAIN_ENTRYPOINT_ID,
  validateDefinition,
  type AutomationDefinition,
  type BlockDef,
} from "../automations/engine/definition.ts";
import { previewDefinition } from "../automations/engine/preview.ts";
import { registerEngineBlocks } from "../automations/engine/blocks/index.ts";
import { makeCodeBlockRuntime } from "../automations/code/runtime.ts";
import {
  draftActionCatalog,
  draftBlockCatalog,
  draftEventCatalog,
  type DraftEventCatalogDeps,
} from "../automations/draft-catalog.ts";
import { AUTOMATION_DRAFT_TASK_TYPE } from "../automations/draft.ts";
import type { ToolContext, ToolRegistry } from "./registry.ts";

export const DRAFT_DEFINITION_MAX_CHARS = 512 * 1024;

export interface AutomationDraftToolDeps {
  store: Pick<
    AutomationStore,
    "getByDraftSession" | "saveVersion" | "updateMeta" | "list"
  >;
  profiles: Pick<ProfileStore, "getActive" | "list">;
  events: DraftEventCatalogDeps;
  now?: () => Date;
}

const DRAFT_TASK_TYPES = [AUTOMATION_DRAFT_TASK_TYPE] as const;

const ReadInput = z.object({
  part: z.enum(["catalog", "events", "actions", "profiles", "draft", "org_automations"]),
});
const ReadOutput = z.object({ part: z.string(), content: z.unknown() });

const ProposeInput = z.object({
  definition_json: z.string().min(1).max(DRAFT_DEFINITION_MAX_CHARS),
  expected_version: z.number().int().min(1),
  note: z.string().max(2_000).optional(),
});
const ProposeError = z.object({
  block_id: z.string(),
  field: z.string(),
  message: z.string(),
});
const ProposeOutput = z.object({
  applied: z.boolean(),
  new_version: z.number().optional(),
  current_version: z.number().optional(),
  /** The live definition on a version conflict, so the agent merges instead
   * of guessing. */
  definition_json: z.string().optional(),
  errors: z.array(ProposeError).optional(),
});

const SetMetaInput = z.object({
  name: z.string().min(1).max(200).optional(),
  description: z.string().max(2_000).optional(),
});
const SetMetaOutput = z.object({ applied: z.boolean() });

const TestInput = z.object({
  payload_json: z.string().max(DRAFT_DEFINITION_MAX_CHARS).optional(),
  event_key: z.string().optional(),
  entrypoint_id: z.string().optional(),
});
const TestOutput = z.object({
  blocks: z.array(
    z.object({
      block_id: z.string(),
      block_type: z.string(),
      rendered_json: z.string(),
      filter_pass: z.boolean().optional(),
    }),
  ),
  errors: z.array(
    z.object({ block_id: z.string(), field: z.string(), code: z.string(), message: z.string() }),
  ),
});

class DraftBindingError extends Error {}

async function requireDraft(
  ctx: ToolContext,
  deps: AutomationDraftToolDeps,
): Promise<AutomationRow> {
  const row = await deps.store.getByDraftSession(ctx.sessionId);
  if (!row) {
    throw new DraftBindingError(
      "this session is not bound to a draft automation (the draft may have been detached)",
    );
  }
  return row;
}

function walkBlocks(list: readonly BlockDef[]): BlockDef[] {
  const out: BlockDef[] = [];
  for (const block of list) {
    out.push(block);
    if (block.then) out.push(...walkBlocks(block.then));
    if (block.else) out.push(...walkBlocks(block.else));
    if (block.body) out.push(...walkBlocks(block.body));
  }
  return out;
}

/** The cheap slice of session-block validation (profile existence). The
 * full harness/model matrix still guards SaveVersion and snapshot time. */
async function checkProfiles(
  definition: AutomationDefinition,
  profiles: Pick<ProfileStore, "getActive">,
): Promise<Array<z.infer<typeof ProposeError>>> {
  const errors: Array<z.infer<typeof ProposeError>> = [];
  const lists = [definition.blocks, ...(definition.entrypoints ?? []).map((e) => e.blocks)];
  for (const block of lists.flatMap((list) => walkBlocks(list))) {
    if (block.type !== "create_session") continue;
    const profileId = block.config["profileId"];
    if (typeof profileId !== "string" || profileId === "") continue;
    if (!(await profiles.getActive(profileId))) {
      errors.push({
        block_id: block.id,
        field: "profileId",
        message: `"${profileId}" is not an active profile (read part "profiles")`,
      });
    }
  }
  return errors;
}

export function registerAutomationDraftTools(
  registry: ToolRegistry,
  deps: AutomationDraftToolDeps,
): void {
  registry.register({
    name: "automation_read",
    taskTypes: DRAFT_TASK_TYPES,
    description:
      "Read drafting context: 'catalog' = the block types and their exact config schemas; " +
      "'events' = trigger events with real sample payloads per provider; 'actions' = " +
      "integration actions; 'profiles' = the profiles a create_session block can run; " +
      "'draft' = the automation you are drafting (current definition + version); " +
      "'org_automations' = what already exists, each with its FULL effective definition — " +
      "read these to avoid duplicating one and to learn the house patterns. Recon with " +
      "catalog/events/org_automations before your first propose.",
    input: ReadInput,
    output: ReadOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      switch (args.part) {
        case "catalog":
          return { part: args.part, content: draftBlockCatalog() };
        case "events":
          return { part: args.part, content: await draftEventCatalog(deps.events) };
        case "actions":
          return { part: args.part, content: await draftActionCatalog(deps.events.connectors) };
        case "profiles": {
          const profiles = await deps.profiles.list({ includeArchived: false });
          return {
            part: args.part,
            content: profiles.map((profile) => ({
              id: profile.id,
              name: profile.name,
              description: profile.description,
              repos: profile.repos.map((repo) =>
                repo.remote ? `${repo.remote.owner}/${repo.remote.name}` : repo.path,
              ),
            })),
          };
        }
        case "draft": {
          const row = await requireDraft(ctx, deps);
          return {
            part: args.part,
            content: {
              automation_id: row.id,
              name: row.name,
              description: row.description,
              enabled: row.enabled,
              version: row.currentVersion,
              definition: definitionOf(row.version),
            },
          };
        }
        case "org_automations": {
          const rows = await deps.store.list({ includeArchived: false });
          const mine = await deps.store.getByDraftSession(ctx.sessionId);
          return {
            part: args.part,
            content: rows
              .filter((row) => row.id !== mine?.id)
              .map((row) => ({
                name: row.name,
                description: row.description,
                enabled: row.enabled,
                // The EFFECTIVE definition (block overrides applied) — what
                // actually runs, so the agent learns real house patterns.
                definition: effectiveDefinition(row.version, row.blockOverrides),
              })),
          };
        }
      }
    },
  });

  registry.register({
    name: "automation_propose",
    taskTypes: DRAFT_TASK_TYPES,
    description:
      "Save a new version of the draft automation: the FULL definition JSON " +
      "({engine, trigger, blocks, entrypoints?, inputsSchema, settings}) — this covers " +
      "creating, editing, and deleting blocks, the trigger, entrypoints, inputs, and " +
      "settings in one call. expected_version must equal the current version; on a " +
      "conflict (the person edited in their Builder) you get applied:false with the " +
      "live definition — merge their intent, never overwrite. Validation failures come " +
      "back as applied:false with block/field-addressed errors: fix and re-propose.",
    input: ProposeInput,
    output: ProposeOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const row = await requireDraft(ctx, deps);
      if (args.expected_version !== row.currentVersion) {
        return {
          applied: false,
          current_version: row.currentVersion,
          definition_json: JSON.stringify(definitionOf(row.version)),
          errors: [
            {
              block_id: "",
              field: "expected_version",
              message: `the automation is at version ${row.currentVersion} (the person may have edited it); re-read and merge`,
            },
          ],
        };
      }
      let parsed: unknown;
      try {
        parsed = JSON.parse(args.definition_json);
      } catch (error) {
        return {
          applied: false,
          errors: [
            {
              block_id: "",
              field: "definition_json",
              message: `not valid JSON: ${error instanceof Error ? error.message : String(error)}`,
            },
          ],
        };
      }
      registerEngineBlocks();
      let definition: AutomationDefinition;
      try {
        definition = validateDefinition(parsed, { kind: "user" });
      } catch (error) {
        if (error instanceof DefinitionError) {
          return {
            applied: false,
            errors: [
              { block_id: error.blockId ?? "", field: error.field, message: error.message },
            ],
          };
        }
        throw error;
      }
      const profileErrors = await checkProfiles(definition, deps.profiles);
      if (profileErrors.length > 0) {
        return { applied: false, errors: profileErrors };
      }
      // The fence rides INSIDE saveVersion's FOR UPDATE transaction — the
      // pre-check above is only a fast path; a human save landing between
      // the read and the write throws here instead of being clobbered.
      let saved: AutomationRow | null;
      try {
        saved = await deps.store.saveVersion(row.id, definition, ctx.userId ?? null, undefined, {
          expectedVersion: args.expected_version,
        });
      } catch (error) {
        if (error instanceof SaveVersionConflictError) {
          const fresh = await deps.store.getByDraftSession(ctx.sessionId);
          return {
            applied: false,
            current_version: error.currentVersion,
            ...(fresh
              ? { definition_json: JSON.stringify(definitionOf(fresh.version)) }
              : {}),
            errors: [
              {
                block_id: "",
                field: "expected_version",
                message: `the automation advanced to version ${error.currentVersion} while you were proposing (the person saved an edit); re-read and merge`,
              },
            ],
          };
        }
        throw error;
      }
      if (!saved) {
        throw new DraftBindingError("the draft automation disappeared while saving");
      }
      return { applied: true, new_version: row.currentVersion + 1 };
    },
  });

  registry.register({
    name: "automation_set_meta",
    taskTypes: DRAFT_TASK_TYPES,
    description: "Name and describe the draft automation (shown in the person's list).",
    input: SetMetaInput,
    output: SetMetaOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const row = await requireDraft(ctx, deps);
      await deps.store.updateMeta(row.id, {
        ...(args.name !== undefined ? { name: args.name } : {}),
        ...(args.description !== undefined ? { description: args.description } : {}),
      });
      return { applied: true };
    },
  });

  registry.register({
    name: "automation_test",
    taskTypes: DRAFT_TASK_TYPES,
    description:
      "Render the current draft against a sample event (side-effect free): every Liquid " +
      "template resolves and filters report pass/fail. payload_json is the trigger " +
      "payload (omit for cron/manual). Run this before telling the person the draft is " +
      "ready.",
    input: TestInput,
    output: TestOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const row = await requireDraft(ctx, deps);
      const definition = definitionOf(row.version);
      const entrypointId = args.entrypoint_id ?? MAIN_ENTRYPOINT_ID;
      const entrypoint = entrypointOf(definition, entrypointId);
      if (!entrypoint) {
        return {
          blocks: [],
          errors: [
            {
              block_id: "",
              field: "entrypoint_id",
              code: "unknown_entrypoint",
              message: `entrypoint "${entrypointId}" is not in the draft`,
            },
          ],
        };
      }
      let payload: Record<string, unknown> = {};
      if (args.payload_json !== undefined && args.payload_json !== "") {
        try {
          const parsed: unknown = JSON.parse(args.payload_json);
          if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
            throw new Error("payload must be a JSON object");
          }
          payload = parsed as Record<string, unknown>;
        } catch (error) {
          return {
            blocks: [],
            errors: [
              {
                block_id: "",
                field: "payload_json",
                code: "invalid_payload",
                message: error instanceof Error ? error.message : String(error),
              },
            ],
          };
        }
      }
      registerEngineBlocks();
      const now = deps.now ?? (() => new Date());
      const receivedAt = now().toISOString();
      const result = await previewDefinition({
        definition,
        inputs: resolveInputs(definition),
        automationId: row.id,
        automationName: row.name,
        entrypointId: entrypoint.id,
        trigger: {
          kind: entrypoint.trigger.kind,
          receivedAt,
          ...(args.event_key !== undefined ? { eventKey: args.event_key } : {}),
          ...(entrypoint.trigger.kind === "cron" ? { scheduledFor: receivedAt } : {}),
          payload,
        },
        aliases: [],
        code: makeCodeBlockRuntime(),
      });
      return {
        blocks: result.blocks.map((block) => ({
          block_id: block.blockId,
          block_type: block.blockType,
          rendered_json: JSON.stringify(block.rendered),
          ...(block.filterPass !== undefined ? { filter_pass: block.filterPass } : {}),
        })),
        errors: result.errors.map((error) => ({
          block_id: error.blockId,
          field: error.field,
          code: error.code,
          message: error.message,
        })),
      };
    },
  });
}

function resolveInputs(definition: AutomationDefinition): Record<string, unknown> {
  const resolved: Record<string, unknown> = Object.create(null);
  for (const field of definition.inputsSchema) {
    if (field.default !== undefined) resolved[field.key] = field.default;
  }
  return resolved;
}
