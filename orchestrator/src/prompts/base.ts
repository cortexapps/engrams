import { PAPERCUT_SYSTEM_PROMPT } from "../tools/papercut-prompt.ts";

/** The agent's words are a product surface: they land in Slack threads, PR
 *  descriptions, and commit messages, read by non-native speakers, by
 *  translation, and by other agents. ASD-STE100 is a controlled-language
 *  standard the model already knows, so we cite it instead of restating its
 *  rules. Keep AGENTS.md's "Writing" bullet in sync. */
export const WRITING_STYLE_SYSTEM_PROMPT = `## Writing style
Adhere to ASD-STE100 (Simplified Technical English) in all communications, including written artifacts, code comments, and messages with the user.`;

/** Appended to EVERY session's `ENGRAM_APPEND_SYSTEM_PROMPT` (ADR 0060), after
 *  any trigger-specific prompt. Tests assert against this composed value. */
export const BASE_SYSTEM_PROMPT = [
  WRITING_STYLE_SYSTEM_PROMPT,
  PAPERCUT_SYSTEM_PROMPT,
].join("\n\n");
