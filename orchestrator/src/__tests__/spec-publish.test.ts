import { describe, expect, test } from "bun:test";

import { makeSpecPublishRoute } from "../routes/spec-publish.ts";
import {
  SpecPublishError,
  type RequestPublishInput,
  type SpecPublishRecord,
  type SpecPublishService,
  type SpecPublishStatus,
} from "../specs/publish.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000001160";
const ACTION_ID = "00000000-0000-4000-8000-000000001161";
const OWNER = "owner-1";
const NOW = new Date("2026-08-13T21:00:00.000Z");

function record(): SpecPublishRecord {
  return {
    specId: SPEC_ID,
    sessionId: "session-1",
    checkpointId: "checkpoint-1",
    artifactId: "artifact-1",
    artifactVersion: null,
    state: "requested",
    requestedBy: OWNER,
    requestedAt: NOW,
    acknowledgedQuestionCount: 0,
    acknowledgedQuestionIds: [],
    gapCheckRunId: null,
    attempts: 0,
    nextAttemptAt: NOW,
    lastError: null,
    pinnedAt: null,
    completedAt: null,
  };
}

function status(overrides: Partial<SpecPublishStatus> = {}): SpecPublishStatus {
  return {
    phase: "drafting",
    canPublish: true,
    openQuestions: [],
    publish: null,
    ...overrides,
  };
}

function route(input?: {
  current?: SpecPublishStatus;
  refusal?: SpecPublishError;
  userId?: string;
  member?: boolean;
}) {
  const current = input?.current ?? status();
  const requests: RequestPublishInput[] = [];
  const wakes: string[] = [];
  const publish: Pick<SpecPublishService, "status" | "requestPublish"> = {
    async status() {
      return current;
    },
    async requestPublish(request) {
      requests.push(request);
      if (input?.refusal) throw input.refusal;
      return { publish: record(), status: current, created: true };
    },
  };
  const userId = input?.userId ?? OWNER;
  const app = makeSpecPublishRoute({
    publish,
    wake: async (specId) => {
      wakes.push(specId);
    },
    resolveMembership: async (specId, candidate) =>
      (input?.member ?? true) && specId === SPEC_ID && candidate === userId,
    getSession: async () => ({ user: { id: userId } }),
  });
  return { app, requests, wakes };
}

function post(app: ReturnType<typeof route>["app"], body: Record<string, unknown>) {
  return app.request(`/api/v1/specs/${SPEC_ID}/publish`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
}

describe("spec publish route", () => {
  test("GET returns only the light-confirm fields", async () => {
    const current = status({
      openQuestions: [
        {
          id: "question-1",
          sectionId: "data",
          sectionTitle: "Data model",
          text: "Which clock wins?",
        },
      ],
    });

    const response = await route({ current }).app.request(`/api/v1/specs/${SPEC_ID}/publish`);

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      phase: "drafting",
      canPublish: true,
      openQuestions: [
        {
          id: "question-1",
          sectionId: "data",
          sectionTitle: "Data model",
          text: "Which clock wins?",
        },
      ],
      publish: null,
    });
  });

  test("POST accepts only the action and open-question acknowledgment", async () => {
    const testRoute = route();

    const response = await post(testRoute.app, {
      actionId: ACTION_ID,
      acknowledgeOpenQuestions: true,
    });

    expect(response.status).toBe(202);
    expect(testRoute.requests).toEqual([
      {
        specId: SPEC_ID,
        actorUserId: OWNER,
        actionId: ACTION_ID,
        acknowledgeOpenQuestions: true,
      },
    ]);
    expect(testRoute.wakes).toEqual([SPEC_ID]);
  });

  test("maps ideation to 409", async () => {
    const current = status({ phase: "ideation", canPublish: false });
    const refusal = new SpecPublishError(
      "ideation",
      "Start drafting before you publish this spec.",
      current,
    );

    const response = await post(route({ current, refusal }).app, {
      actionId: ACTION_ID,
      acknowledgeOpenQuestions: false,
    });

    expect(response.status).toBe(409);
    expect(await response.json()).toMatchObject({ reason: "ideation" });
  });

  test("the missing acknowledgment refusal names the open-question count", async () => {
    const current = status({
      openQuestions: [
        { id: "q-1", sectionId: "data", sectionTitle: "Data model", text: "First?" },
        { id: "q-2", sectionId: "data", sectionTitle: "Data model", text: "Second?" },
      ],
    });
    const refusal = new SpecPublishError(
      "acknowledgment_required",
      "2 open questions need an acknowledgment.",
      current,
    );

    const response = await post(route({ current, refusal }).app, {
      actionId: ACTION_ID,
      acknowledgeOpenQuestions: false,
    });

    expect(response.status).toBe(409);
    expect(await response.json()).toMatchObject({
      error: "2 open questions need an acknowledgment.",
      reason: "acknowledgment_required",
    });
  });

  test("refuses a member who is not the owner", async () => {
    const current = status();
    const refusal = new SpecPublishError(
      "not_owner",
      "Only the spec owner can publish this spec.",
      current,
    );

    const response = await post(route({ current, refusal, userId: "member-1" }).app, {
      actionId: ACTION_ID,
      acknowledgeOpenQuestions: false,
    });

    expect(response.status).toBe(403);
    expect(await response.json()).toMatchObject({ reason: "not_owner" });
  });
});
