/**
 * Closing the talk-it-through stage from the canvas (ADR 0114 D6, R22).
 *
 * The notes themselves ride the sync socket, so the canvas needs no read route:
 * it renders the live document. Only the exit needs a verb. "Draft the spec"
 * distils the tagged clusters and archives the notes; the same call with
 * nothing tagged is the skip, because it archives and writes no section.
 */

import { SpecWorkingNotesError } from "@engrams/spec-document";
import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";

import { SpecDocumentReadOnlyError, SpecNotesArchivedError } from "../specs/doc-service.ts";
import type { SpecWorkingNotesService } from "../specs/notes.ts";
import type { GetSession, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

export interface SpecNotesRouteDeps {
  notes: Pick<SpecWorkingNotesService, "distill">;
  resolveMembership: ResolveSpecMembership;
  getSession?: GetSession;
}

export function makeSpecNotesRoute(deps: SpecNotesRouteDeps): Hono {
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

  app.post("/api/v1/specs/:id/notes/distill", async (c) => {
    const specId = await requireMember(c);
    try {
      const result = await deps.notes.distill({ specId, clientId: "spec-notes-distill" });
      return c.json({
        applied: result.applied,
        stage: result.stage,
        writtenSectionIds: result.distillation.sections.map((section) => section.sectionId),
        refutedBullets: result.distillation.refutedBullets,
        untaggedBullets: result.distillation.untaggedBullets,
      });
    } catch (error) {
      throw distillError(error);
    }
  });

  return app;
}

function distillError(error: unknown): HTTPException {
  if (error instanceof HTTPException) return error;
  if (error instanceof SpecNotesArchivedError) {
    return new HTTPException(409, { message: error.message });
  }
  if (error instanceof SpecDocumentReadOnlyError) {
    return new HTTPException(409, { message: error.message });
  }
  if (error instanceof SpecWorkingNotesError) {
    return new HTTPException(400, { message: error.message });
  }
  throw error;
}
