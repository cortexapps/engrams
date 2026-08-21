/** Legacy action ↔ block definition mapping (ADR 0119, phase 1).
 *
 * The proto surface stays byte-identical until phase 3: the RPC layer keeps
 * speaking `trigger + create_task action`, and this module converts that
 * shape to/from a one-block definition. Remove with the phase-3 proto break.
 */

import type {
  AutomationAction,
  AutomationTrigger,
  CreateTaskAutomationAction,
} from "../db/schema.ts";
import { ENGINE_VERSION, type AutomationDefinition, type BlockDef } from "./engine/definition.ts";

export const LEGACY_CREATE_SESSION_BLOCK_ID = "create_session";

export function definitionFromLegacyAction(
  trigger: AutomationTrigger,
  action: AutomationAction,
): AutomationDefinition {
  const { kind: _kind, profileId, promptTemplate, ...rest } = action;
  const block: BlockDef = {
    id: LEGACY_CREATE_SESSION_BLOCK_ID,
    type: "create_session",
    config: {
      profileId,
      promptTemplate,
      includeEventContext: action.includeEventContext,
      ...(rest.titleTemplate !== undefined ? { titleTemplate: rest.titleTemplate } : {}),
      ...(rest.harnessMode !== undefined ? { harnessMode: rest.harnessMode } : {}),
      ...(rest.harness !== undefined ? { harness: rest.harness } : {}),
      ...(rest.model !== undefined ? { model: rest.model } : {}),
      ...(rest.modelRouter !== undefined ? { modelRouter: rest.modelRouter } : {}),
      ...(rest.effort !== undefined ? { effort: rest.effort } : {}),
    },
  };
  return {
    engine: ENGINE_VERSION,
    trigger,
    blocks: [block],
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
  };
}

/** Total for anything the legacy surface can create: exactly one
 * create_session block. Null for every other graph (excluded from the legacy
 * proto surface instead of rendered lossily). */
export function legacyActionFromDefinition(
  definition: AutomationDefinition,
): CreateTaskAutomationAction | null {
  if (definition.blocks.length !== 1) return null;
  const block = definition.blocks[0]!;
  if (block.type !== "create_session") return null;
  const config = block.config as {
    profileId?: unknown;
    promptTemplate?: unknown;
    titleTemplate?: unknown;
    includeEventContext?: unknown;
    harnessMode?: unknown;
    harness?: unknown;
    model?: unknown;
    modelRouter?: unknown;
    effort?: unknown;
  };
  if (typeof config.profileId !== "string" || typeof config.promptTemplate !== "string") {
    return null;
  }
  return {
    kind: "create_task",
    profileId: config.profileId,
    promptTemplate: config.promptTemplate,
    includeEventContext: config.includeEventContext === true,
    ...(typeof config.titleTemplate === "string" ? { titleTemplate: config.titleTemplate } : {}),
    ...(typeof config.harnessMode === "string" ? { harnessMode: config.harnessMode } : {}),
    ...(typeof config.harness === "string" ? { harness: config.harness } : {}),
    ...(typeof config.model === "string" ? { model: config.model } : {}),
    ...(typeof config.modelRouter === "string" ? { modelRouter: config.modelRouter } : {}),
    ...(typeof config.effort === "string" ? { effort: config.effort } : {}),
  };
}

/** Map engine run statuses onto the strings the current web page renders.
 * Remove in phase 3 with the proto break. */
export function legacyRunStatus(status: string): string {
  switch (status) {
    case "completed":
      return "launched";
    case "filtered":
    case "superseded":
    case "halted":
      return "skipped";
    case "failed":
    case "deadline":
      return "launch_failed";
    case "pending":
    case "running":
    case "waiting":
      return "pending";
    default:
      return status;
  }
}
