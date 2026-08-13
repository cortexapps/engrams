import { randomUUID } from "node:crypto";

import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";
import type { Pool } from "pg";

import { sessions as defaultSessions } from "../control-plane/client.ts";
import type { GetSession, GuardUser, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";
import { speakerName, type SpecMessageClient } from "./spec-messages.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

export type SpecPhase = "ideation" | "drafting" | "published";

export interface StartDraftingResult {
  phase: SpecPhase;
  started: boolean;
  sessionId: string | null;
}

export interface SpecStartDraftingStore {
  start(specId: string, at: Date): Promise<StartDraftingResult | null>;
}

interface SpecPhaseRow {
  phase: string;
  session_id: string | null;
}

/** Compare and set the one-way ideation-to-drafting transition. */
export class PostgresSpecStartDraftingStore implements SpecStartDraftingStore {
  constructor(private readonly pool: Pool) {}

  async start(specId: string, at: Date): Promise<StartDraftingResult | null> {
    const updated = await this.pool.query<SpecPhaseRow>(
      `UPDATE spec
          SET phase = 'drafting', updated_at = $2
        WHERE id = $1 AND phase = 'ideation' AND session_id IS NOT NULL
      RETURNING phase, session_id`,
      [specId, at],
    );
    const transitioned = updated.rows[0];
    if (transitioned) {
      return { phase: "drafting", started: true, sessionId: transitioned.session_id };
    }

    const current = await this.pool.query<SpecPhaseRow>(
      "SELECT phase, session_id FROM spec WHERE id = $1",
      [specId],
    );
    const row = current.rows[0];
    if (!row) return null;
    return { phase: parsePhase(row.phase, specId), started: false, sessionId: row.session_id };
  }
}

export interface SpecStartDraftingRouteDeps {
  store: SpecStartDraftingStore;
  resolveMembership: ResolveSpecMembership;
  preparePrompt(sessionId: string, status: string): Promise<void>;
  sessions?: SpecMessageClient;
  getSession?: GetSession;
  randomId?: () => string;
  now?: () => Date;
}

export function makeSpecStartDraftingRoute(deps: SpecStartDraftingRouteDeps): Hono {
  const app = new Hono();
  const authorize = makeSpecMemberHeaderGuard(deps.resolveMembership, deps.getSession);
  const client = deps.sessions ?? defaultSessions;
  const randomId = deps.randomId ?? randomUUID;
  const now = deps.now ?? (() => new Date());

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

  app.post("/api/v1/specs/:id/start-drafting", async (c) => {
    const { specId, user } = await requireMember(c);
    const transition = await deps.store.start(specId, now());
    if (!transition) throw new HTTPException(404, { message: "not found" });
    if (transition.phase === "published") {
      throw new HTTPException(409, { message: "a published spec cannot return to drafting" });
    }
    if (!transition.started) {
      if (transition.phase === "ideation") {
        throw new HTTPException(409, { message: "the spec has no drafting session" });
      }
      return c.json({ phase: "drafting", started: false });
    }
    if (!transition.sessionId) {
      throw new Error(`Spec ${specId} entered drafting without a session.`);
    }

    const session = await client.getSession({ sessionId: transition.sessionId });
    await deps.preparePrompt(transition.sessionId, session.session?.status ?? "");
    const promptId = `spec-start-drafting:${randomId()}`;
    await client.sendPrompt({
      sessionId: transition.sessionId,
      promptId,
      text: `[start drafting — requested by ${speakerName(user.name)}]`,
    });
    return c.json({ phase: "drafting", started: true, prompt_id: promptId });
  });

  return app;
}

function parsePhase(value: string, specId: string): SpecPhase {
  if (value === "ideation" || value === "drafting" || value === "published") return value;
  throw new Error(`Spec ${specId} has an invalid phase: ${value}`);
}
