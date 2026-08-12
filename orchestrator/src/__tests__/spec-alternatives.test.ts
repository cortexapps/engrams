import type {
  AlternativesDecidedTranscriptChip,
  AlternativesProposedTranscriptChip,
  SpecAlternativesStage,
} from "@engrams/spec-document";
import { SpecAlternativesError } from "@engrams/spec-document";
import { afterEach, describe, expect, mock, test } from "bun:test";
import { Hono } from "hono";

import {
  makeSpecAlternativesRoute,
  type SpecAlternativesRouteDeps,
} from "../routes/spec-alternatives.ts";
import { SpecAlternativesConflictError } from "../specs/alternatives.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000001120";

const proposal: AlternativesProposedTranscriptChip = {
  kind: "spec_alternatives_proposed",
  specId: SPEC_ID,
  sectionId: "alternatives",
  setId: "set-1",
  options: [
    {
      key: "A",
      title: "Second org-level bucket",
      tradeoffs: [
        { sign: "+", text: "no schema change on the hot path" },
        { sign: "-", text: "two buckets to reason about on refusal" },
        { sign: "~", text: "billing still needs a separate meter path" },
      ],
    },
    {
      key: "B",
      title: "Hierarchical limiter",
      tradeoffs: [
        { sign: "+", text: "one code path" },
        { sign: "-", text: "touches every gateway call site" },
        { sign: "~", text: "meter emission fits at the walk root" },
      ],
    },
  ],
  comparison: {
    provenance: "verified against gateway/limits.rs @ 8f2c1a4",
    rows: [
      {
        axis: "Blast radius",
        cells: [
          { optionKey: "A", value: "2 files" },
          { optionKey: "B", value: "31 call sites" },
        ],
      },
    ],
  },
  leanKey: "B",
};

const decided: AlternativesDecidedTranscriptChip = {
  kind: "spec_alternatives_decided",
  specId: SPEC_ID,
  sectionId: "alternatives",
  setId: "set-1",
  pickedKey: "B",
  reason: "One code path.",
  decidedBy: "author",
};

type DecideInput = Parameters<SpecAlternativesRouteDeps["alternatives"]["decide"]>[0];

function testApp(input?: {
  stage?: SpecAlternativesStage | null;
  decide?: (
    value: DecideInput,
  ) => Promise<{ stage: SpecAlternativesStage; applied: boolean; newRev: bigint }>;
  resolveMembership?: (specId: string, userId: string) => Promise<boolean>;
}) {
  const app = new Hono();
  const stage: SpecAlternativesStage | null =
    input && "stage" in input ? (input.stage ?? null) : { proposal, decision: null };
  const readStage = mock(async () => stage);
  const decide = mock(
    input?.decide ??
      (async (_value: DecideInput) => ({
        stage: { proposal, decision: decided },
        applied: true,
        newRev: 9n,
      })),
  );
  app.route(
    "/",
    makeSpecAlternativesRoute({
      alternatives: { readStage, decide },
      resolveMembership:
        input?.resolveMembership ??
        (async (specId, userId) => specId === SPEC_ID && userId === "member-2"),
      getSession: async () => ({ user: { id: "member-2", name: "Grace" } }),
    }),
  );
  return { app, decide, readStage };
}

afterEach(() => mock.restore());

describe("spec alternatives routes", () => {
  test("returns the live stage for a member", async () => {
    const { app } = testApp();
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/alternatives`);

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ stage: { proposal, decision: null } });
  });

  test("returns a null stage before the agent proposes anything", async () => {
    const { app } = testApp({ stage: null });
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/alternatives`);

    expect(await response.json()).toEqual({ stage: null });
  });

  test("hides the stage from a person outside the org", async () => {
    const { app } = testApp({ resolveMembership: async () => false });
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/alternatives`);

    expect(response.status).toBe(404);
  });

  test("records the pick as an author decision", async () => {
    const { app, decide } = testApp();
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/alternatives/decide`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ setId: "set-1", optionKey: "B", reason: "One code path." }),
    });

    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({
      applied: true,
      stage: { decision: { pickedKey: "B" } },
    });
    expect(decide.mock.calls[0]![0]).toMatchObject({
      specId: SPEC_ID,
      setId: "set-1",
      optionKey: "B",
      decidedBy: "author",
    });
  });

  test("carries a hybrid pick through with a null option", async () => {
    const { app, decide } = testApp();
    await app.request(`/api/v1/specs/${SPEC_ID}/alternatives/decide`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ setId: "set-1", optionKey: null, reason: "Take A's meter path." }),
    });

    expect(decide.mock.calls[0]![0]).toMatchObject({ optionKey: null });
  });

  test("refuses a pick with no reason", async () => {
    const { app, decide } = testApp();
    const response = await app.request(`/api/v1/specs/${SPEC_ID}/alternatives/decide`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ setId: "set-1", optionKey: "B", reason: "  " }),
    });

    expect(response.status).toBe(400);
    expect(decide).not.toHaveBeenCalled();
  });

  test("maps a stale set to 409 and an invalid option to 400", async () => {
    const stale = testApp({
      decide: async (_value: DecideInput) => {
        throw new SpecAlternativesConflictError("This set is no longer current.");
      },
    });
    expect(
      (
        await stale.app.request(`/api/v1/specs/${SPEC_ID}/alternatives/decide`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ setId: "old", optionKey: "B", reason: "why" }),
        })
      ).status,
    ).toBe(409);

    const unknown = testApp({
      decide: async (_value: DecideInput) => {
        throw new SpecAlternativesError("The pick names an unknown option: Z.");
      },
    });
    expect(
      (
        await unknown.app.request(`/api/v1/specs/${SPEC_ID}/alternatives/decide`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ setId: "set-1", optionKey: "Z", reason: "why" }),
        })
      ).status,
    ).toBe(400);
  });
});
