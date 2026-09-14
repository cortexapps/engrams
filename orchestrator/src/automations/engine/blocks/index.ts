/** Registers every v1 block executor. Imported once at boot (and by the
 * definition validator's callers); the registry itself is static.
 */

import { registerControlBlocks } from "./control.ts";
import { registerSessionBlocks } from "./session.ts";
import { registerWaitEventBlock } from "./wait-event.ts";
import { registerExecBlocks } from "./exec.ts";
import { registerCodeBlock } from "./code.ts";
import { registerStateBlocks } from "./state.ts";
import { registerPrLookupBlock } from "./pr-lookup.ts";
import { registerInstanceCloseBlock } from "./instance-close.ts";
import { registerClaimHandleBlock } from "./claim-handle.ts";
import { registerIntegrationActionBlock } from "./integration-action.ts";
import { registerReviewBlocks } from "./review.ts";
import { registerSlackRelayBlock } from "./relay.ts";
import { registerRelayCloseBlock } from "./relay-close.ts";
import { registerResolveUserBlock } from "./resolve-user.ts";
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
  registerStateBlocks();
  registerPrLookupBlock();
  registerInstanceCloseBlock();
  registerClaimHandleBlock();
  registerIntegrationActionBlock();
  registerReviewBlocks();
  registerSlackRelayBlock();
  registerRelayCloseBlock();
  registerResolveUserBlock();
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
  "session_status",
  "state_get",
  "state_set",
  "state_delete",
  "state_list",
  "lookup_pr_session",
  "instance_close",
  "claim_handle",
  "review_open_pass",
  "review_stage",
  "review_settle",
  "review_close_pass",
  "resolve_user",
  "relay_session",
  "relay_close",
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
