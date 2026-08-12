import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";

import {
  GapCheckError,
  type DispositionAction,
  type GapCheckRun,
  type GapCheckService,
} from "../specs/gap-check.ts";
import type { GetSession, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

/** The wire shape of a run. `rev` is a string because it is a bigint. */
export interface SpecGapCheckRunPayload {
  id: string;
  specId: string;
  rev: string;
  stoppedAtLayerKey: string | null;
  suppressedCount: number;
  matrix: GapCheckRun["matrix"];
  findings: Array<{
    id: string;
    kind: string;
    severity: string;
    layerKey: string;
    sectionId: string;
    sectionTitle: string;
    requirementId: string | null;
    summary: string;
    detail: string;
    proposedDiff: { sectionId: string; before: string; after: string } | null;
    disposition: string;
    openQuestionId: string | null;
    disposedAt: string | null;
  }>;
  createdAt: string;
}

export interface SpecGapCheckRouteDeps {
  gapCheck: Pick<GapCheckService, "status" | "run" | "readRun" | "disposeFinding">;
  resolveMembership: ResolveSpecMembership;
  getSession?: GetSession;
}

export function makeSpecGapCheckRoute(deps: SpecGapCheckRouteDeps): Hono {
  const app = new Hono();
  const authorize = makeSpecMemberHeaderGuard(deps.resolveMembership, deps.getSession);

  async function requireMember(c: Context): Promise<{ specId: string; userId: string }> {
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
    return { specId, userId: result.user.id };
  }

  app.get("/api/v1/specs/:id/gap-check", async (c) => {
    const { specId } = await requireMember(c);
    try {
      const status = await deps.gapCheck.status(specId);
      return c.json({
        stale: status.stale,
        rev: status.currentSemanticDocSeq.toString(),
        run: status.run === null ? null : runPayload(status.run),
      });
    } catch (error) {
      throw gapCheckHttpError(error);
    }
  });

  app.post("/api/v1/specs/:id/gap-check", async (c) => {
    const { specId, userId } = await requireMember(c);
    const body = await readJsonObject(c);
    const actionId = body["actionId"];
    if (typeof actionId !== "string" || !UUID.test(actionId)) {
      throw new HTTPException(400, { message: "actionId must be a UUID" });
    }
    try {
      // A person runs the traceability pass. Red-team findings need agent
      // judgment, so they arrive through the spec_gap_check tool instead.
      const run = await deps.gapCheck.run({
        specId,
        sessionId: null,
        requestFingerprint: `browser-gap-check:${actionId}`,
        actorUserId: userId,
      });
      return c.json({ run: runPayload(run) });
    } catch (error) {
      throw gapCheckHttpError(error);
    }
  });

  app.post("/api/v1/specs/:id/gap-check/:runId/findings/:findingId", async (c) => {
    const { userId } = await requireMember(c);
    const runId = c.req.param("runId");
    if (typeof runId !== "string" || !UUID.test(runId)) {
      throw new HTTPException(404, { message: "not found" });
    }
    const findingId = c.req.param("findingId");
    if (typeof findingId !== "string" || findingId.length === 0 || findingId.length > 400) {
      throw new HTTPException(404, { message: "not found" });
    }
    const body = await readJsonObject(c);
    const action = requireAction(body["action"]);
    try {
      const run = await deps.gapCheck.disposeFinding({
        runId,
        findingId,
        action,
        actorUserId: userId,
      });
      return c.json({ run: runPayload(run) });
    } catch (error) {
      throw gapCheckHttpError(error);
    }
  });

  return app;
}

function runPayload(run: GapCheckRun): SpecGapCheckRunPayload {
  return {
    id: run.id,
    specId: run.specId,
    rev: run.semanticDocSeq.toString(),
    stoppedAtLayerKey: run.stoppedAtLayerKey,
    suppressedCount: run.suppressedCount,
    matrix: run.matrix,
    findings: run.findings.map((finding) => ({
      id: finding.id,
      kind: finding.kind,
      severity: finding.severity,
      layerKey: finding.layerKey,
      sectionId: finding.sectionId,
      sectionTitle: finding.sectionTitle,
      requirementId: finding.requirementId,
      summary: finding.summary,
      detail: finding.detail,
      proposedDiff: finding.proposedDiff,
      disposition: finding.disposition,
      openQuestionId: finding.openQuestionId,
      disposedAt: finding.disposedAt === null ? null : finding.disposedAt.toISOString(),
    })),
    createdAt: run.createdAt.toISOString(),
  };
}

function requireAction(value: unknown): DispositionAction {
  if (value !== "open_question" && value !== "accept_diff" && value !== "dismiss") {
    throw new HTTPException(400, {
      message: "action must be open_question, accept_diff, or dismiss",
    });
  }
  return value;
}

async function readJsonObject(c: Context): Promise<Record<string, unknown>> {
  let value: unknown;
  try {
    value = await c.req.json();
  } catch {
    throw new HTTPException(400, { message: "invalid JSON body" });
  }
  if (!isRecord(value)) throw new HTTPException(400, { message: "body must be an object" });
  return value;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function gapCheckHttpError(error: unknown): HTTPException {
  if (error instanceof HTTPException) return error;
  if (error instanceof GapCheckError) {
    switch (error.code) {
      case "spec_not_found":
      case "run_not_found":
      case "finding_not_found":
        return new HTTPException(404, { message: error.message });
      case "already_disposed":
      case "stale_run":
        return new HTTPException(409, { message: error.message });
      default:
        return new HTTPException(400, { message: error.message });
    }
  }
  throw error;
}
