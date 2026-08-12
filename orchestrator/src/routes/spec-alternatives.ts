/**
 * The alternatives stage in the canvas (ADR 0114 D6, requirement R20).
 *
 * The canvas reads the current set here and posts the pick here. The pick
 * writes §Alternatives considered through the ordinary section mutation.
 */

import { SpecAlternativesError } from "@engrams/spec-document";
import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";

import {
  SpecAlternativesConflictError,
  type SpecAlternativesService,
} from "../specs/alternatives.ts";
import { SpecDocumentReadOnlyError } from "../specs/doc-service.ts";
import type { GetSession, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

export interface SpecAlternativesRouteDeps {
  alternatives: Pick<SpecAlternativesService, "readStage" | "decide">;
  resolveMembership: ResolveSpecMembership;
  getSession?: GetSession;
}

export function makeSpecAlternativesRoute(deps: SpecAlternativesRouteDeps): Hono {
  const app = new Hono();
  const authorize = makeSpecMemberHeaderGuard(deps.resolveMembership, deps.getSession);

  async function requireMember(c: Context): Promise<string> {
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
    return specId;
  }

  app.get("/api/v1/specs/:id/alternatives", async (c) => {
    const specId = await requireMember(c);
    return c.json({ stage: await deps.alternatives.readStage(specId) });
  });

  app.post("/api/v1/specs/:id/alternatives/decide", async (c) => {
    const specId = await requireMember(c);
    const body = await readJsonObject(c);
    const setId = body["setId"];
    const optionKey = body["optionKey"];
    const reason = body["reason"];
    if (typeof setId !== "string" || setId.length === 0 || setId.length > 200) {
      throw new HTTPException(400, { message: "setId must be a non-empty string" });
    }
    if (optionKey !== undefined && optionKey !== null && typeof optionKey !== "string") {
      throw new HTTPException(400, { message: "optionKey must be a string or null" });
    }
    if (typeof reason !== "string" || reason.trim().length === 0) {
      throw new HTTPException(400, { message: "reason must be a non-empty string" });
    }
    try {
      const result = await deps.alternatives.decide({
        specId,
        setId,
        optionKey: typeof optionKey === "string" ? optionKey : null,
        reason,
        decidedBy: "author",
        actionId: setId,
      });
      return c.json({ stage: result.stage, applied: result.applied });
    } catch (error) {
      throw decideError(error);
    }
  });

  return app;
}

async function readJsonObject(c: Context): Promise<Record<string, unknown>> {
  let value: unknown;
  try {
    value = await c.req.json();
  } catch {
    throw new HTTPException(400, { message: "invalid JSON body" });
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new HTTPException(400, { message: "body must be an object" });
  }
  return value as Record<string, unknown>;
}

function decideError(error: unknown): HTTPException {
  if (error instanceof HTTPException) return error;
  if (error instanceof SpecAlternativesConflictError) {
    return new HTTPException(409, { message: error.message });
  }
  if (error instanceof SpecDocumentReadOnlyError) {
    return new HTTPException(409, { message: error.message });
  }
  if (error instanceof SpecAlternativesError) {
    return new HTTPException(400, { message: error.message });
  }
  throw error;
}
