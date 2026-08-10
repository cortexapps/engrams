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

/** Appended to every session's `ENGRAM_APPEND_SYSTEM_PROMPT` (ADR 0060), after
 *  any trigger-specific prompt. Context-specific instructions are added by
 *  `systemPromptForTaskType`. */
export const BASE_SYSTEM_PROMPT = [
  WRITING_STYLE_SYSTEM_PROMPT,
  PAPERCUT_SYSTEM_PROMPT,
].join("\n\n");

/** Build the prompt in the orchestrator. The sandbox receives only the final
 *  prompt and does not know the task type that selected it. */
export function systemPromptForTaskType(taskType?: string): string {
  return [
    BASE_SYSTEM_PROMPT,
    ...(taskType === "spec" ? [SPEC_MODE_SYSTEM_PROMPT] : []),
  ].join("\n\n");
}
