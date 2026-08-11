import { PAPERCUT_SYSTEM_PROMPT } from "../tools/papercut-prompt.ts";

/** The agent's words are a product surface: they land in Slack threads, PR
 *  descriptions, and commit messages, read by non-native speakers, by
 *  translation, and by other agents. ASD-STE100 is a controlled-language
 *  standard the model already knows, so we cite it instead of restating its
 *  rules. Keep AGENTS.md's "Writing" bullet in sync. */
export const WRITING_STYLE_SYSTEM_PROMPT = `## Writing style
Adhere to ASD-STE100 (Simplified Technical English) in all communications, including written artifacts, code comments, and messages with the user.`;

/** Spec sessions use the same instruction for every harness. The live read at
 *  the start of each turn avoids stale disk projections after queued prompts. */
export const SPEC_MODE_SYSTEM_PROMPT = `## Spec mode
When /workspace/spec.md exists, the session has a collaborative spec. At the start of every turn, call spec_read before you reason about or change the spec. spec_read is the live source of truth. Read /workspace/.engrams/spec/digest.md when you need the human-change summary.

Treat /workspace/spec.md as a read-only projection. Never edit it with file or shell tools. Use the spec_* tools for every spec change.`;

/** The multi-agent playbook a coordinating session needs but no single tool
 *  owns. The per-tool contract (one child per unit of work, reuse over
 *  respawn, the read cadence) lives in the tool descriptions, where the model
 *  reads it at the call site; this carries the rules that span several tools
 *  and several turns. Keep it in sync with AGENTS.md's "Multi-PR and
 *  multi-agent work" section, which states the same rules for this repo. */
export const COORDINATION_SYSTEM_PROMPT = `## Sub-session coordination
A child session is durable. It has its own sandbox, it keeps its own history, and it continues after your turn ends. Use a child session for work that must survive this turn, change a repository, or open a pull request.

Spawn one child for each unit of work, such as one issue or one pull request. Send the follow-up work and the review fixes to that same child. Do not spawn a second child for the review, the rebase, or the wait on continuous integration.

Keep the shared state in this session. Do the rebases, the branch bases, and the order of the merges yourself. Reserve the migration numbers before the children start. When pull requests depend on each other, keep only the next one in merge order green.

Only the newest commit of a pull request is live. Ignore the results and the review findings of a commit that a later push replaced. When an automated reviewer already comments on a pull request, read its findings yourself and give the valid ones to the child that owns the branch.

Give the user a plan of the children and their units of work before you spawn them, and tell the user when the set of children changes.`;

/** Appended to every session's `ENGRAM_APPEND_SYSTEM_PROMPT` (ADR 0060), after
 *  any trigger-specific prompt. Context-specific instructions are added by
 *  `systemPromptForTaskType`. */
export const BASE_SYSTEM_PROMPT = [
  WRITING_STYLE_SYSTEM_PROMPT,
  PAPERCUT_SYSTEM_PROMPT,
].join("\n\n");

/** Build the prompt in the orchestrator. The sandbox receives only the final
 *  prompt and does not know the task type or the tool manifest that selected
 *  it.
 *
 *  `toolNames` is the compiled manifest for THIS session, so the coordination
 *  playbook follows the coordination tools instead of restating their gating.
 *  A session that cannot spawn never reads how to spawn. */
export function systemPromptForTaskType(
  taskType?: string,
  toolNames: readonly string[] = [],
): string {
  return [
    BASE_SYSTEM_PROMPT,
    ...(taskType === "spec" ? [SPEC_MODE_SYSTEM_PROMPT] : []),
    ...(toolNames.includes("spawn_session") ? [COORDINATION_SYSTEM_PROMPT] : []),
  ].join("\n\n");
}
