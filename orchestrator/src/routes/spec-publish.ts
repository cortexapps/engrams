import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";

import {
  SpecPublishError,
  type SpecPublishRecord,
  type SpecPublishService,
  type SpecPublishStatus,
} from "../specs/publish.ts";
import type { GetSession, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

/** The wire shape of the gate. Every org member may read it (ADR 0114 D12). */
export interface SpecPublishStatusPayload {
  lifecycle: "draft" | "published";
  canPublish: boolean;
  publishedAt: string | null;
  gate: {
    ready: boolean;
    settledRequiredCount: number;
    requiredCount: number;
    acknowledgmentRequired: boolean;
    gapCheckRunRequired: boolean;
    blockers: Array<{
      sectionId: string;
      sectionTitle: string;
      layerKey: string;
      state: string;
      reason: string;
    }>;
    openQuestions: Array<{
      id: string;
      sectionId: string;
      sectionTitle: string;
      text: string;
    }>;
  };
  gapCheck: { stale: boolean; runId: string | null; ranAt: string | null; gates: boolean };
  publish: {
    state: string;
    checkpointId: string;
    artifactId: string;
    artifactVersion: number | null;
    acknowledgedQuestionCount: number;
    requestedAt: string;
    pinnedAt: string | null;
    completedAt: string | null;
    lastError: string | null;
  } | null;
}

export interface SpecPublishRouteDeps {
  publish: Pick<SpecPublishService, "status" | "requestPublish">;
  /**
   * The push wake for the lifecycle scanner. The route records the intent and
   * nudges; it never runs the publish steps itself (ADR 0034).
   */
  wake: (specId: string) => Promise<unknown>;
  resolveMembership: ResolveSpecMembership;
  getSession?: GetSession;
}

export function makeSpecPublishRoute(deps: SpecPublishRouteDeps): Hono {
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

  app.get("/api/v1/specs/:id/publish", async (c) => {
    const { specId, userId } = await requireMember(c);
    try {
      return c.json(statusPayload(await deps.publish.status(specId, userId)));
    } catch (error) {
      throw publishHttpError(error);
    }
  });

  app.post("/api/v1/specs/:id/publish", async (c) => {
    const { specId, userId } = await requireMember(c);
    const body = await readJsonObject(c);
    const actionId = body["actionId"];
    if (typeof actionId !== "string" || !UUID.test(actionId)) {
      throw new HTTPException(400, { message: "actionId must be a UUID" });
    }
    const acknowledgeOpenQuestions = flag(body["acknowledgeOpenQuestions"], "acknowledgeOpenQuestions");
    const runGapCheck = flag(body["runGapCheck"], "runGapCheck");

    let result;
    try {
      result = await deps.publish.requestPublish({
        specId,
        actorUserId: userId,
        actionId,
        acknowledgeOpenQuestions,
        runGapCheck,
      });
    } catch (error) {
      throw publishHttpError(error);
    }
    // Best effort: the timer sweep is the durability backstop, so a wake that
    // fails costs latency, never the publish.
    await deps.wake(specId).catch(() => undefined);
    const status = await deps.publish.status(specId, userId);
    return c.json(statusPayload(status), result.created ? 202 : 200);
  });

  return app;
}

function statusPayload(status: SpecPublishStatus): SpecPublishStatusPayload {
  return {
    lifecycle: status.lifecycle,
    canPublish: status.canPublish,
    publishedAt: status.publishedAt?.toISOString() ?? null,
    gate: {
      ready: status.gate.ready,
      settledRequiredCount: status.gate.settledRequiredCount,
      requiredCount: status.gate.requiredCount,
      acknowledgmentRequired: status.gate.acknowledgmentRequired,
      gapCheckRunRequired: status.gate.gapCheckRunRequired,
      blockers: status.gate.blockers.map((blocker) => ({
        sectionId: blocker.sectionId,
        sectionTitle: blocker.sectionTitle,
        layerKey: blocker.layerKey,
        state: blocker.state,
        reason: blocker.reason,
      })),
      openQuestions: status.gate.openQuestions.map((question) => ({
        id: question.id,
        sectionId: question.sectionId,
        sectionTitle: question.sectionTitle,
        text: question.text,
      })),
    },
    gapCheck: {
      stale: status.gapCheck.stale,
      runId: status.gapCheck.runId,
      ranAt: status.gapCheck.ranAt?.toISOString() ?? null,
      gates: status.gapCheck.gates,
    },
    publish: status.publish === null ? null : publishPayload(status.publish),
  };
}

function publishPayload(publish: SpecPublishRecord) {
  return {
    state: publish.state,
    checkpointId: publish.checkpointId,
    artifactId: publish.artifactId,
    artifactVersion: publish.artifactVersion,
    acknowledgedQuestionCount: publish.acknowledgedQuestionCount,
    requestedAt: publish.requestedAt.toISOString(),
    pinnedAt: publish.pinnedAt?.toISOString() ?? null,
    completedAt: publish.completedAt?.toISOString() ?? null,
    lastError: publish.lastError,
  };
}

function flag(value: unknown, field: string): boolean {
  if (value === undefined) return false;
  if (typeof value !== "boolean") {
    throw new HTTPException(400, { message: `${field} must be true or false` });
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

/**
 * A refusal carries the gate, so the dialog lists the blockers or the questions
 * without a second request.
 */
function publishHttpError(error: unknown): HTTPException {
  if (error instanceof HTTPException) return error;
  if (error instanceof SpecPublishError) {
    const body = {
      error: error.message,
      reason: error.code,
      ...(error.status ? { status: statusPayload(error.status) } : {}),
    };
    switch (error.code) {
      case "spec_not_found":
        return new HTTPException(404, { message: error.message });
      // The spec is readable by every org member, so a non-owner learns that
      // they cannot publish rather than that the spec does not exist (R37).
      case "not_owner":
        return httpJson(403, body);
      case "blocked":
      case "acknowledgment_required":
      case "gap_check_stale":
      case "gap_check_failed":
      case "already_published":
      case "no_session":
        return httpJson(409, body);
    }
  }
  throw error;
}

function httpJson(status: 403 | 409, body: unknown): HTTPException {
  return new HTTPException(status, {
    res: new Response(JSON.stringify(body), {
      status,
      headers: { "content-type": "application/json" },
    }),
  });
}
