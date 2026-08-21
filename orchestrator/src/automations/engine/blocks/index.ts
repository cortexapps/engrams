/** Registers every v1 block executor. Imported once at boot (and by the
 * definition validator's callers); the registry itself is static.
 */

import { registerControlBlocks } from "./control.ts";
import { registerSessionBlocks } from "./session.ts";
import { registerWaitEventBlock } from "./wait-event.ts";
import { registerExecBlocks } from "./exec.ts";
import { registerCodeBlock } from "./code.ts";
import { registerIntegrationActionBlock } from "./integration-action.ts";
import { getBlock, listBlockTypes } from "./registry.ts";

let registered = false;

export function registerEngineBlocks(): void {
  if (registered) return;
  registered = true;
  registerControlBlocks();
  registerSessionBlocks();
  registerWaitEventBlock();
  registerExecBlocks();
  registerCodeBlock();
  registerIntegrationActionBlock();
}

export const V1_BLOCK_TYPES = [
  "filter",
  "branch",
  "loop",
  "code",
  "create_session",
  "send_prompt",
  "wait_session",
  "wait_event",
  "end_session",
  "run_command",
  "write_files",
  "integration_action",
] as const;

/** Boot assertion (next to assertSweepPoliciesExhaustive): every v1 type is
 * registered, so a definition that validated at save time always executes. */
export function assertBlockRegistryComplete(): void {
  registerEngineBlocks();
  const missing = V1_BLOCK_TYPES.filter((type) => getBlock(type) === undefined);
  if (missing.length > 0) {
    throw new Error(`block registry incomplete: missing ${missing.join(", ")} (have ${listBlockTypes().join(", ")})`);
  }
}
