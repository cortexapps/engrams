import { PAPERCUT_SYSTEM_PROMPT } from "../tools/papercut-prompt.ts";
import { specModeSystemPrompt, type SpecPromptContext } from "./spec-mode.ts";

/** The agent's words are a product surface: they land in Slack threads, PR
 *  descriptions, and commit messages, read by non-native speakers, by
 *  translation, and by other agents. ASD-STE100 is a controlled-language
 *  standard the model already knows, so we cite it instead of restating its
 *  rules. Keep AGENTS.md's "Writing" bullet in sync. */
export const WRITING_STYLE_SYSTEM_PROMPT = `## Writing style
Adhere to ASD-STE100 (Simplified Technical English) in all communications, including written artifacts, code comments, and messages with the user.`;

/** Appended to every session's `ENGRAM_APPEND_SYSTEM_PROMPT` (ADR 0060), after
 *  any trigger-specific prompt. Context-specific instructions are added by
 *  `systemPromptForTaskType`. */
export const BASE_SYSTEM_PROMPT = [
  WRITING_STYLE_SYSTEM_PROMPT,
  PAPERCUT_SYSTEM_PROMPT,
].join("\n\n");

/** Build the prompt in the orchestrator. The sandbox receives only the final
 *  prompt and does not know the task type that selected it.
 *
 *  `spec` is the spec's template snapshot. It shapes the spec-mode instruction
 *  (ADR 0114 D6) and is ignored for every other task type. A spec session
 *  without a snapshot still gets the standing rules of the mode. */
export function systemPromptForTaskType(taskType?: string, spec?: SpecPromptContext): string {
  return [
    BASE_SYSTEM_PROMPT,
    ...(taskType === "spec" ? [specModeSystemPrompt(spec)] : []),
  ].join("\n\n");
}
