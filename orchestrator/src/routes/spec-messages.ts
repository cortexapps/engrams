import { randomUUID } from "node:crypto";

import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";
import type { Pool } from "pg";

import { sessions as defaultSessions } from "../control-plane/client.ts";
import type { GetSession, GuardUser, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";

const MAX_MESSAGE_BYTES = 20_000;
const MESSAGE_PAGE_SIZE = 500;
const MAX_SPEAKER_NAME_CHARS = 80;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
const ISO_TIMESTAMP =
  /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/;

export interface SpecChatMessageRecord {
  promptId: string;
  specId: string;
  authorUserId: string | null;
  authorName: string;
  text: string;
  createdAt: Date;
}

export interface CreateSpecChatMessageInput {
  promptId: string;
  specId: string;
  authorUserId: string;
  authorName: string;
  text: string;
}

export interface SpecMessageStore {
  resolveSessionId(specId: string): Promise<string | null>;
  insertMessage(input: CreateSpecChatMessageInput): Promise<void>;
  listMessages(
    specId: string,
    after: Date | undefined,
    limit: number,
  ): Promise<SpecChatMessageRecord[]>;
}

interface SpecSessionRow {
  session_id: string | null;
}

interface SpecChatMessageRow {
  prompt_id: string;
  spec_id: string;
  author_user_id: string | null;
  author_name: string;
  text: string;
  created_at: Date;
}

export class PostgresSpecMessageStore implements SpecMessageStore {
  constructor(private readonly pool: Pool) {}

  async resolveSessionId(specId: string): Promise<string | null> {
    const result = await this.pool.query<SpecSessionRow>(
      "SELECT session_id FROM spec WHERE id = $1",
      [specId],
    );
    return result.rows[0]?.session_id ?? null;
  }

  async insertMessage(input: CreateSpecChatMessageInput): Promise<void> {
    await this.pool.query(
      `INSERT INTO spec_chat_message
         (prompt_id, spec_id, author_user_id, author_name, text)
       VALUES ($1, $2, $3, $4, $5)`,
      [input.promptId, input.specId, input.authorUserId, input.authorName, input.text],
    );
  }

  async listMessages(
    specId: string,
    after: Date | undefined,
    limit: number,
  ): Promise<SpecChatMessageRecord[]> {
    const result = after
      ? await this.pool.query<SpecChatMessageRow>(
          `SELECT prompt_id, spec_id, author_user_id, author_name, text, created_at
             FROM spec_chat_message
            WHERE spec_id = $1 AND created_at > $2
            ORDER BY created_at ASC, prompt_id ASC
            LIMIT $3`,
          [specId, after, limit],
        )
      : await this.pool.query<SpecChatMessageRow>(
          `SELECT prompt_id, spec_id, author_user_id, author_name, text, created_at
             FROM spec_chat_message
            WHERE spec_id = $1
            ORDER BY created_at ASC, prompt_id ASC
            LIMIT $2`,
          [specId, limit],
        );
    return result.rows.map((row) => ({
      promptId: row.prompt_id,
      specId: row.spec_id,
      authorUserId: row.author_user_id,
      authorName: row.author_name,
      text: row.text,
      createdAt: row.created_at,
    }));
  }
}

export interface SpecMessageClient {
  getSession(input: { sessionId: string }): Promise<{ session?: { status: string } }>;
  sendPrompt(input: { sessionId: string; promptId: string; text: string }): Promise<unknown>;
}

export interface SpecMessagesRouteDeps {
  store: SpecMessageStore;
  resolveMembership: ResolveSpecMembership;
  preparePrompt(sessionId: string, status: string): Promise<void>;
  sessions?: SpecMessageClient;
  getSession?: GetSession;
  randomId?: () => string;
}

interface MessageBody {
  message?: unknown;
}

export function makeSpecMessagesRoute(deps: SpecMessagesRouteDeps): Hono {
  const app = new Hono();
  const authorize = makeSpecMemberHeaderGuard(deps.resolveMembership, deps.getSession);
  const client = deps.sessions ?? defaultSessions;
  const randomId = deps.randomId ?? randomUUID;

  async function requireMember(c: Context): Promise<{ specId: string; user: GuardUser }> {
    const specId = c.req.param("id");
    if (typeof specId !== "string" || !UUID.test(specId)) {
      throw new HTTPException(404, { message: "not found" });
    }
    const result = await authorize(c.req.raw.headers, specId);
    if (!result.ok) {
      throw new HTTPException(result.status, {
        message: result.status === 401 ? "unauthenticated" : "not found",
      });
    }
    return { specId, user: result.user };
  }

  app.post("/api/v1/specs/:id/messages", async (c) => {
    const { specId, user } = await requireMember(c);
    let body: MessageBody;
    try {
      body = await c.req.json<MessageBody>();
    } catch {
      throw new HTTPException(400, { message: "request body must be JSON" });
    }
    const message = typeof body.message === "string" ? body.message.trim() : "";
    if (!message) throw new HTTPException(400, { message: "message is required" });
    if (new TextEncoder().encode(message).byteLength > MAX_MESSAGE_BYTES) {
      throw new HTTPException(413, { message: `message exceeds ${MAX_MESSAGE_BYTES} bytes` });
    }

    const sessionId = await deps.store.resolveSessionId(specId);
    if (!sessionId) throw new HTTPException(404, { message: "not found" });
    const session = await client.getSession({ sessionId });
    await deps.preparePrompt(sessionId, session.session?.status ?? "");

    const promptId = `spec-chat:${randomId()}`;
    const authorName = speakerName(user.name);
    // Persist first. An event consumer can then always join the agent reply to
    // the clean human turn as soon as the prompt reaches the shared session.
    await deps.store.insertMessage({
      promptId,
      specId,
      authorUserId: user.id,
      authorName,
      text: message,
    });
    await client.sendPrompt({
      sessionId,
      promptId,
      text: `[speaker: ${authorName}]\n${message}`,
    });
    return c.json({ prompt_id: promptId }, 202);
  });

  app.get("/api/v1/specs/:id/messages", async (c) => {
    const { specId } = await requireMember(c);
    const after = parseAfter(c.req.query("after"));
    const messages = await deps.store.listMessages(specId, after, MESSAGE_PAGE_SIZE);
    return c.json({
      messages: messages.map((message) => ({
        prompt_id: message.promptId,
        author: { id: message.authorUserId, name: message.authorName },
        text: message.text,
        created_at: message.createdAt.toISOString(),
      })),
    });
  });

  return app;
}

/**
 * Make a display name safe to put in the trusted header field.
 *
 * The agent reads `[speaker: <name>]` as the authority for who spoke, so a
 * name must not be able to shape that line. Control codes and line separators
 * go because they could open a second line; brackets go because they could
 * close this header early and open another one on the same line; the length
 * cap keeps a long name from pushing the message out of the model's attention.
 */
export function speakerName(name: string | undefined): string {
  const clean = (name ?? "")
    .replace(/[\p{Cc}\p{Zl}\p{Zp}[\]]/gu, "")
    .trim()
    .slice(0, MAX_SPEAKER_NAME_CHARS);
  return clean || "Unknown member";
}

function parseAfter(value: string | undefined): Date | undefined {
  if (value === undefined || value.trim() === "") return undefined;
  if (!ISO_TIMESTAMP.test(value)) {
    throw new HTTPException(400, { message: "after must be an ISO timestamp" });
  }
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) {
    throw new HTTPException(400, { message: "after must be an ISO timestamp" });
  }
  return parsed;
}
