/**
 * Auto-titles a task from the first human prompt of its primary session.
 *
 * The coordinator echoes every prompt into the session log as an
 * `agent_message` with `role:"user"` (api/prompt.rs), and curation forwards it
 * (session-events.ts). This consumer reacts to the first one on a human-owned
 * root task's primary session, asks a small model for a 3-7 word title, and writes
 * `task.suggested_title` — the slot `effectiveTitle` (rpc/tasks.ts) already
 * prefers over the truncated-prompt default while still losing to a user
 * rename (`custom_title`).
 *
 * Replay safety: delivery is at-least-once, so the "already titled" gate runs
 * BEFORE the model call (no paid call per replay) and the write is a
 * conditional UPDATE on `suggested_title IS NULL`.
 *
 * Retry policy: a transient model failure (429, 5xx, network, an unreachable
 * coordinator) throws, and the pump redelivers with its 250ms→30s backoff —
 * capped at MAX_MODEL_ATTEMPTS paid calls per event, then the consumer gives
 * up and the truncated-prompt default stays. A permanent failure (bad key,
 * bad model id) gives up on the first call. The cap lives in memory on
 * purpose: a listener restart redelivers the event with a fresh budget.
 */

import { and, eq, isNull, ne } from "drizzle-orm";
import { APICallError, generateObject, RetryError } from "ai";
import { z } from "zod";

import type { CuratedEvent } from "../control-plane/session-events.ts";
import { getDb } from "../db/client.ts";
import { task as taskTable, taskSession as taskSessionTable } from "../db/schema.ts";
import { getOpenRouterClient } from "../integrations/openrouter.ts";
import { log as rootLog } from "../log.ts";
import type { SessionConsumer } from "./consumer.ts";

const log = rootLog.child({ component: "title-consumer" });

const TITLE_MODEL = "deepseek/deepseek-v4-flash";
/** Bound on the prompt text sent to the titling model. */
const PROMPT_SLICE_CHARS = 2000;

const TITLE_SYSTEM_PROMPT = `Generate a concise, sentence-case title (3-7 words) that captures the main topic or goal of this coding session. The title should be clear enough that the user recognizes the session in a list. Use sentence case: capitalize only the first word and proper nouns.

Return JSON with a single "title" field.

Good examples:
{"title": "Fix login button on mobile"}
{"title": "Add OAuth authentication"}
{"title": "Debug failing CI tests"}
{"title": "Refactor API client error handling"}

Bad (too vague): {"title": "Code changes"}
Bad (too long): {"title": "Investigate and fix the issue where the login button does not respond on mobile devices"}
Bad (wrong case): {"title": "Fix Login Button On Mobile"}`;

const titleSchema = z.object({ title: z.string().trim().min(1).max(120) });

/** The user prompt echo's text, or undefined for everything else (assistant
 *  turns, system notes, blank prompts, malformed payloads). */
export function parseUserPrompt(event: CuratedEvent): string | undefined {
  if (event.kind !== "agent_message") return undefined;
  try {
    const p = JSON.parse(event.payloadJson) as { role?: unknown; text?: unknown };
    return p?.role === "user" && typeof p.text === "string" && p.text.trim() !== ""
      ? p.text
      : undefined;
  } catch {
    return undefined;
  }
}

export interface TitleableTask {
  taskId: string;
  suggestedTitle: string | null;
  customTitle: string | null;
}

export interface TitleConsumerDeps {
  /** The task behind `sessionId`'s PRIMARY task_session, when a human owns it
   *  (`created_by_user_id` set — the tool-consumer's ownerless test, inverted).
   *  Null for child tasks, review workers, ownerless automations, and
   *  unattributed sessions. */
  findTitleableTask(sessionId: string): Promise<TitleableTask | null>;
  /** Ask the model for the title. May throw. */
  generateTitle(prompt: string): Promise<string>;
  /** Write the title iff `suggested_title` is still NULL. */
  saveSuggestedTitle(taskId: string, title: string): Promise<void>;
  /** True for an error a retry cannot fix (bad key, bad model id). */
  isPermanentError(err: unknown): boolean;
}

/** Max paid model calls per event before the consumer gives up on it. */
const MAX_MODEL_ATTEMPTS = 8;

export function makeTitleConsumer(deps: TitleConsumerDeps): SessionConsumer {
  // The per-event retry budget (see the retry policy in the header).
  let attemptEventIdx = -1n;
  let attempts = 0;
  return {
    name: "title",
    interestedIn: (kind) => kind === "agent_message",
    async appliesTo(sessionId) {
      return (await deps.findTitleableTask(sessionId)) !== null;
    },
    async handle(event, ctx) {
      const prompt = parseUserPrompt(event);
      if (prompt === undefined) return;
      // Re-read per event: appliesTo ran once at listener start, and the gate
      // must see a title written by an earlier event or a user rename.
      const task = await deps.findTitleableTask(ctx.sessionId);
      if (task === null || task.suggestedTitle != null || task.customTitle != null) return;
      let title: string;
      try {
        title = await deps.generateTitle(prompt);
      } catch (err) {
        if (event.idx !== attemptEventIdx) {
          attemptEventIdx = event.idx;
          attempts = 0;
        }
        attempts += 1;
        if (!deps.isPermanentError(err) && attempts < MAX_MODEL_ATTEMPTS) {
          throw err; // the pump redelivers this event with 250ms→30s backoff
        }
        log.warn(
          { sessionId: ctx.sessionId, taskId: task.taskId, attempts, err },
          "title generation failed; keeping the default title",
        );
        return;
      }
      await deps.saveSuggestedTitle(task.taskId, title);
      log.info(
        { sessionId: ctx.sessionId, taskId: task.taskId, title },
        "auto-titled task from its first prompt",
      );
    },
  };
}

export function makeProductionTitleConsumer(): SessionConsumer {
  const db = getDb();
  return makeTitleConsumer({
    async findTitleableTask(sessionId) {
      const rows = await db
        .select({
          taskId: taskTable.id,
          suggestedTitle: taskTable.suggestedTitle,
          customTitle: taskTable.customTitle,
          createdByUserId: taskTable.createdByUserId,
        })
        .from(taskSessionTable)
        .innerJoin(taskTable, eq(taskSessionTable.taskId, taskTable.id))
        .where(
          and(
            eq(taskSessionTable.sessionId, sessionId),
            eq(taskSessionTable.role, "primary"),
            ne(taskTable.type, "subsession"),
          ),
        )
        .limit(1);
      const row = rows[0];
      return row !== undefined && row.createdByUserId != null
        ? { taskId: row.taskId, suggestedTitle: row.suggestedTitle, customTitle: row.customTitle }
        : null;
    },
    async generateTitle(prompt) {
      const openrouter = await getOpenRouterClient();
      const { object } = await generateObject({
        model: openrouter.chat(TITLE_MODEL),
        schema: titleSchema,
        system: TITLE_SYSTEM_PROMPT,
        prompt: prompt.slice(0, PROMPT_SLICE_CHARS),
      });
      return object.title;
    },
    async saveSuggestedTitle(taskId, title) {
      await db
        .update(taskTable)
        .set({ suggestedTitle: title })
        .where(and(eq(taskTable.id, taskId), isNull(taskTable.suggestedTitle)));
    },
    isPermanentError,
  });
}

/** Permanent = an API rejection a retry cannot fix: an `APICallError` the SDK
 *  itself marks non-retryable (401 bad key, 400/404 bad model id). The SDK's
 *  internal retries surface as a `RetryError`; classify by its last cause.
 *  Everything else (429/5xx, network, an unreachable coordinator) is worth the
 *  pump's backoff — the attempt cap bounds the spend either way. */
export function isPermanentError(err: unknown): boolean {
  const cause = RetryError.isInstance(err) ? err.lastError : err;
  return APICallError.isInstance(cause) && !cause.isRetryable;
}
