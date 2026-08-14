import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";

import {
  type SpecDraftingSeedStore,
  type SpecPhase,
  type StartDraftingResult,
} from "../specs/drafting-seed-scanner.ts";
import type { GetSession, GuardUser, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";
import { speakerName } from "./spec-messages.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

export type { SpecPhase, StartDraftingResult };
export type SpecStartDraftingStore = Pick<SpecDraftingSeedStore, "start">;

export interface SpecStartDraftingRouteDeps {
  store: SpecStartDraftingStore;
  resolveMembership: ResolveSpecMembership;
  /** Best-effort push wake. The periodic scanner is the durability backstop. */
  wake(specId: string): Promise<unknown>;
  getSession?: GetSession;
  now?: () => Date;
}

export function makeSpecStartDraftingRoute(deps: SpecStartDraftingRouteDeps): Hono {
  const app = new Hono();
  const authorize = makeSpecMemberHeaderGuard(deps.resolveMembership, deps.getSession);
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
    const transition = await deps.store.start({
      specId,
      at: now(),
      text: `[start drafting — requested by ${speakerName(user.name)}]`,
    });
    if (!transition) throw new HTTPException(404, { message: "not found" });
    if (transition.phase === "published") {
      throw new HTTPException(409, { message: "a published spec cannot return to drafting" });
    }
    if (!transition.started) {
      if (transition.phase === "ideation") {
        throw new HTTPException(409, { message: "the spec has no drafting session" });
      }
      await deps.wake(specId).catch(() => undefined);
      return c.json({ phase: "drafting", started: false });
    }
    if (!transition.sessionId || !transition.promptId) {
      throw new Error(`Spec ${specId} entered drafting without a durable seed.`);
    }

    await deps.wake(specId).catch(() => undefined);
    return c.json({ phase: "drafting", started: true, prompt_id: transition.promptId });
  });

  return app;
}
