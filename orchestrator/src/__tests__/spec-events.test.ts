import { describe, expect, test } from "bun:test";

import { makeEventsRoute, type SessionsClient } from "../routes/events.ts";
import { makeSpecEventsRoute } from "../routes/spec-events.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000001140";
const SESSION_ID = "00000000-0000-4000-8000-000000001141";

function finiteClient(
  events: Array<{ idx?: bigint; kind: string; payloadJson: string }>,
  calls: Array<{ sessionId: string; since?: bigint }>,
): SessionsClient {
  return {
    async *streamEvents(request) {
      calls.push(request);
      yield* events;
    },
  };
}

describe("spec events route", () => {
  test("uses the shared SSE envelope, keeps idx zero, and omits id for an absent idx", async () => {
    const calls: Array<{ sessionId: string; since?: bigint }> = [];
    const app = makeSpecEventsRoute({
      resolveMembership: async () => true,
      resolveSessionId: async () => SESSION_ID,
      getSession: async () => ({ user: { id: "member-1", name: "Ada" } }),
      sessions: finiteClient(
        [
          { idx: 0n, kind: "run_started", payloadJson: '{"run_id":"run-1"}' },
          { kind: "lagged", payloadJson: '{"missed":2}' },
        ],
        calls,
      ),
    });

    const response = await app.request(`/api/v1/specs/${SPEC_ID}/events`);
    const body = await response.text();

    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toContain("text/event-stream");
    expect(calls).toEqual([{ sessionId: SESSION_ID, since: undefined }]);
    const startedFrame = body
      .split("\n\n")
      .find((frame) => frame.includes("event: run_started"));
    expect(startedFrame).toBeDefined();
    expect(startedFrame).toContain("id: 0");
    expect(startedFrame).toContain(
      'data: {"idx":0,"kind":"run_started","payload_json":"{\\"run_id\\":\\"run-1\\"}"}',
    );
    const laggedFrame = body.split("\n\n").find((frame) => frame.includes("event: lagged"));
    expect(laggedFrame).toBeDefined();
    expect(laggedFrame).not.toContain("id:");
    expect(laggedFrame).toContain(
      'data: {"idx":null,"kind":"lagged","payload_json":"{\\"missed\\":2}"}',
    );
  });

  test("takes the larger BigInt cursor without a Number round trip", async () => {
    const calls: Array<{ sessionId: string; since?: bigint }> = [];
    const app = makeSpecEventsRoute({
      resolveMembership: async () => true,
      resolveSessionId: async () => SESSION_ID,
      getSession: async () => ({ user: { id: "member-1" } }),
      sessions: finiteClient([], calls),
    });

    const response = await app.request(
      `/api/v1/specs/${SPEC_ID}/events?since=9007199254740993`,
      { headers: { "Last-Event-ID": "9007199254740994" } },
    );
    await response.text();

    expect(response.status).toBe(200);
    expect(calls).toEqual([{ sessionId: SESSION_ID, since: 9007199254740994n }]);
  });

  test("accepts members and rejects unauthenticated users and non-members", async () => {
    const sessions = finiteClient([], []);
    const unauthenticated = makeSpecEventsRoute({
      resolveMembership: async () => true,
      resolveSessionId: async () => SESSION_ID,
      getSession: async () => null,
      sessions,
    });
    const nonMember = makeSpecEventsRoute({
      resolveMembership: async () => false,
      resolveSessionId: async () => SESSION_ID,
      getSession: async () => ({ user: { id: "outsider" } }),
      sessions,
    });

    expect((await unauthenticated.request(`/api/v1/specs/${SPEC_ID}/events`)).status).toBe(401);
    expect((await nonMember.request(`/api/v1/specs/${SPEC_ID}/events`)).status).toBe(404);
  });

  test("keeps the owner-only session route on the shared stream helper", async () => {
    const calls: Array<{ sessionId: string; since?: bigint }> = [];
    const app = makeEventsRoute({
      getSession: async () => ({ user: { id: "owner-1" } }),
      resolveOwner: async () => "owner-1",
      sessions: finiteClient(
        [{ idx: 4n, kind: "run_completed", payloadJson: "{}" }],
        calls,
      ),
    });

    const response = await app.request(`/api/v1/sessions/${SESSION_ID}/events?since=3`);
    const body = await response.text();

    expect(response.status).toBe(200);
    expect(calls).toEqual([{ sessionId: SESSION_ID, since: 3n }]);
    const frame = body.split("\n\n").find((candidate) => candidate.includes("event: run_completed"));
    expect(frame).toContain("id: 4");
  });
});
