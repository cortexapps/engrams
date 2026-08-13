import { SpecWorkingNotesError } from "@engrams/spec-document";
import { afterEach, describe, expect, mock, test } from "bun:test";
import { Hono } from "hono";

import { makeSpecNotesRoute } from "../routes/spec-notes.ts";
import { SpecDocumentReadOnlyError, SpecNotesArchivedError } from "../specs/doc-service.ts";
import type { SpecWorkingNotesService } from "../specs/notes.ts";

const SPEC_ID = "00000000-0000-4000-8000-0000000021a0";

const STAGE = {
  notes: {
    clusters: [
      {
        id: "burst",
        theme: "burst semantics",
        sectionIds: ["behavior"],
        bullets: [
          {
            id: "b1",
            mark: "verified" as const,
            kind: "observation" as const,
            text: "in-flight sessions are sacred",
            provenance: "agreed with the author",
            agentText: "in-flight sessions are sacred",
          },
        ],
      },
    ],
  },
  archivedAt: "2026-08-12T09:30:00.000Z",
  untaggedBullets: 0,
};

function testApp(input?: {
  distill?: (
    input: Parameters<SpecWorkingNotesService["distill"]>[0],
  ) => ReturnType<SpecWorkingNotesService["distill"]>;
  getSession?: () => Promise<{ user: { id: string; name: string } } | null>;
  resolveMembership?: (specId: string, userId: string) => Promise<boolean>;
}) {
  const app = new Hono();
  const distill = mock(
    input?.distill ??
      (async () => ({
        distillation: {
          sections: [{ sectionId: "behavior", markdown: "Verified — it holds\n" }],
          refutedBullets: 1,
          untaggedBullets: 0,
        },
        stage: STAGE,
        applied: true,
        newRev: 9n,
      })),
  );
  app.route(
    "/",
    makeSpecNotesRoute({
      notes: { distill },
      resolveMembership:
        input?.resolveMembership ??
        (async (specId, userId) => specId === SPEC_ID && userId === "member-2"),
      getSession: input?.getSession ?? (async () => ({ user: { id: "member-2", name: "Grace" } })),
    }),
  );
  return { app, distill };
}

async function distillRequest(app: Hono, specId = SPEC_ID): Promise<Response> {
  return app.request(`/api/v1/specs/${specId}/notes/distill`, { method: "POST" });
}

afterEach(() => mock.restore());

describe("the notes distillation route", () => {
  test("an org member closes the stage from the canvas", async () => {
    const { app, distill } = testApp();

    const response = await distillRequest(app);

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      applied: true,
      stage: STAGE,
      writtenSectionIds: ["behavior"],
      refutedBullets: 1,
      untaggedBullets: 0,
    });
    expect(distill).toHaveBeenCalledWith({ specId: SPEC_ID, clientId: "spec-notes-distill" });
  });

  test("an archived stage reports a conflict", async () => {
    const { app } = testApp({
      distill: async () => {
        throw new SpecNotesArchivedError("2026-08-12T09:30:00.000Z");
      },
    });

    expect((await distillRequest(app)).status).toBe(409);
  });

  test("a published spec reports a conflict", async () => {
    const { app } = testApp({
      distill: async () => {
        throw new SpecDocumentReadOnlyError(SPEC_ID);
      },
    });

    expect((await distillRequest(app)).status).toBe(409);
  });

  test("a spec with no notes reports a bad request", async () => {
    const { app } = testApp({
      distill: async () => {
        throw new SpecWorkingNotesError("This spec has no working notes to distil.");
      },
    });

    expect((await distillRequest(app)).status).toBe(400);
  });

  test("the guard hides a spec from a stranger and rejects an anonymous call", async () => {
    const stranger = testApp({ resolveMembership: async () => false });
    const anonymous = testApp({ getSession: async () => null });

    expect((await distillRequest(stranger.app)).status).toBe(404);
    expect((await distillRequest(anonymous.app)).status).toBe(401);
    expect(stranger.distill).not.toHaveBeenCalled();
    expect(anonymous.distill).not.toHaveBeenCalled();
  });

  test("a malformed spec id never reaches the service", async () => {
    const { app, distill } = testApp();

    expect((await distillRequest(app, "not-a-uuid")).status).toBe(404);
    expect(distill).not.toHaveBeenCalled();
  });
});
