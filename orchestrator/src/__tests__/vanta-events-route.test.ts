/**
 * Vanta ingress: Svix-signed deliveries, the five-minute replay window, and
 * the `v1.` prefix the catalog key drops.
 */

import { createHmac } from "node:crypto";
import { describe, expect, test } from "bun:test";

import { makeVantaEventsRoute, vantaEventKey } from "../routes/vanta-events.ts";
import type { RecordIntegrationEventInput } from "../db/integration-events.ts";

const RAW_SECRET = Buffer.from("vanta-test-signing-secret-bytes");
const SECRET = `whsec_${RAW_SECRET.toString("base64")}`;
const NOW = new Date("2026-10-28T17:06:35.000Z");

function sign(id: string, timestamp: string, body: string, secret: Buffer = RAW_SECRET): string {
  return createHmac("sha256", secret).update(`${id}.${timestamp}.${body}`).digest("base64");
}

interface Delivered {
  status: number;
  recorded: RecordIntegrationEventInput[];
}

async function deliver(
  payload: Record<string, unknown>,
  opts: { headers?: Record<string, string>; secret?: Buffer; ageSeconds?: number } = {},
): Promise<Delivered> {
  const recorded: RecordIntegrationEventInput[] = [];
  const body = JSON.stringify(payload);
  const app = makeVantaEventsRoute({
    secrets: { resolve: async () => SECRET },
    now: () => NOW,
    ingress: {
      now: () => NOW,
      connectionIdFor: async () => "conn-vanta",
      store: {
        record: async (input) => {
          recorded.push(input);
          return { recorded: true, id: "evt-1" };
        },
        sweep: async () => 0,
        list: async () => [],
        getLatest: async () => null,
        listObservedEventKeys: async () => [],
        getById: async () => null,
        listObservedScopeValues: async () => [],
      },
      dispatch: async () => ({
        matched: 0, started: 0, joined: 0, queued: 0, skipped: 0, filtered: 0, dropped: 0,
        suppressed: [], failed: 0, builtins: {},
      }),
    },
  });
  const id = `msg_${Math.random().toString(36).slice(2)}`;
  const timestamp = String(Math.floor(NOW.getTime() / 1000) - (opts.ageSeconds ?? 0));
  const res = await app.request("/api/v1/integrations/vanta/events", {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "svix-id": id,
      "svix-timestamp": timestamp,
      "svix-signature": `v1,${sign(id, timestamp, body, opts.secret)}`,
      ...(opts.headers ?? {}),
    },
    body,
  });
  return { status: res.status, recorded };
}

describe("vanta ingress", () => {
  test("a signed delivery ledgers under the catalog key without the v1 prefix", async () => {
    const got = await deliver({
      type: "v1.questionnaire.status-changed",
      questionnaire: { id: "q1", status: "COMPLETE" },
    });
    expect(got.status).toBe(200);
    expect(got.recorded).toHaveLength(1);
    expect(got.recorded[0]!.eventKey).toBe("questionnaire.status-changed");
    expect(got.recorded[0]!.scopeValue).toBeUndefined();
  });

  test("eventType is read when type is absent", async () => {
    const got = await deliver({ eventType: "v1.vendor.decision.created", vendor: { id: "v1" } });
    expect(got.recorded[0]!.eventKey).toBe("vendor.decision.created");
  });

  test("a delivery with no event type is acknowledged and skipped", async () => {
    const got = await deliver({ questionnaire: { id: "q1" } });
    expect(got.status).toBe(200);
    expect(got.recorded).toHaveLength(0);
  });

  test("a wrong secret is refused", async () => {
    const got = await deliver(
      { type: "v1.questionnaire.created", questionnaire: { id: "q1" } },
      { secret: Buffer.from("another-secret") },
    );
    expect(got.status).toBe(401);
    expect(got.recorded).toHaveLength(0);
  });

  test("a delivery older than five minutes is a replay", async () => {
    const fresh = await deliver(
      { type: "v1.questionnaire.created", questionnaire: { id: "q1" } },
      { ageSeconds: 4 * 60 },
    );
    expect(fresh.status).toBe(200);
    const stale = await deliver(
      { type: "v1.questionnaire.created", questionnaire: { id: "q1" } },
      { ageSeconds: 6 * 60 },
    );
    expect(stale.status).toBe(401);
    expect(stale.recorded).toHaveLength(0);
  });

  test("a second signature entry in the header still verifies", async () => {
    const recorded: RecordIntegrationEventInput[] = [];
    const body = JSON.stringify({ type: "v1.evidence.created", evidence: { id: "e1" } });
    const id = "msg_multi";
    const timestamp = String(Math.floor(NOW.getTime() / 1000));
    const app = makeVantaEventsRoute({
      secrets: { resolve: async () => SECRET },
      now: () => NOW,
      ingress: {
        now: () => NOW,
        connectionIdFor: async () => "conn-vanta",
        store: {
          record: async (input) => {
            recorded.push(input);
            return { recorded: true, id: "evt-1" };
          },
          sweep: async () => 0,
          list: async () => [],
          getLatest: async () => null,
          listObservedEventKeys: async () => [],
          getById: async () => null,
          listObservedScopeValues: async () => [],
        },
        dispatch: async () => ({
          matched: 0, started: 0, joined: 0, queued: 0, skipped: 0, filtered: 0, dropped: 0,
          suppressed: [], failed: 0, builtins: {},
        }),
      },
    });
    const res = await app.request("/api/v1/integrations/vanta/events", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "svix-id": id,
        "svix-timestamp": timestamp,
        "svix-signature": `v1,${sign(id, timestamp, body, Buffer.from("rotated-away"))} v1,${sign(id, timestamp, body)}`,
      },
      body,
    });
    expect(res.status).toBe(200);
    expect(recorded[0]!.eventKey).toBe("evidence.created");
  });

  test("a delivery without svix-id is malformed", async () => {
    const got = await deliver(
      { type: "v1.questionnaire.created", questionnaire: { id: "q1" } },
      { headers: { "svix-id": "" } },
    );
    expect([400, 401]).toContain(got.status);
    expect(got.recorded).toHaveLength(0);
  });

  test("vantaEventKey drops the version prefix and refuses junk", () => {
    expect(vantaEventKey("v1.trust-center.access-request.approved")).toBe(
      "trust-center.access-request.approved",
    );
    expect(vantaEventKey("v1.Control.Comment.Created")).toBe("control.comment.created");
    expect(vantaEventKey("v1.")).toBeNull();
    expect(vantaEventKey("a b")).toBeNull();
    expect(vantaEventKey(7)).toBeNull();
  });
});
