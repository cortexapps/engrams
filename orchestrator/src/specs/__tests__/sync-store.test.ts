import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { EventEmitter } from "node:events";
import { readFile } from "node:fs/promises";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool } from "pg";
import * as awarenessProtocol from "y-protocols/awareness";
import * as Y from "yjs";

import * as schema from "../../db/schema.ts";
import { PostgresSpecAwarenessBus } from "../sync-store.ts";
import { PostgresSpecParticipantStore } from "../sync-store.ts";
import { parseSpecChannelEnvelope, SPEC_CHANNEL_PAYLOAD_MAX_BYTES } from "../doc-service.ts";

class FakeAwarenessClient extends EventEmitter {
  listenAttempts = 0;
  releases = 0;
  failNextListen = false;

  async query(queryText: string): Promise<unknown> {
    if (queryText.startsWith("LISTEN")) {
      this.listenAttempts += 1;
      if (this.failNextListen) {
        this.failNextListen = false;
        throw new Error("LISTEN failed");
      }
    }
    return undefined;
  }

  release(): void {
    this.releases += 1;
  }
}

describe("PostgresSpecAwarenessBus", () => {
  test("releases the client and permits a retry when LISTEN fails", async () => {
    const client = new FakeAwarenessClient();
    client.failNextListen = true;
    const pool = {
      connect: async () => client,
      query: async () => {},
    };
    const bus = new PostgresSpecAwarenessBus(pool);
    const handlers = { update: () => {}, query: () => {}, participantConnected: () => {} };

    await expect(bus.start(handlers)).rejects.toThrow("LISTEN failed");
    expect(client.releases).toBe(1);
    expect(client.listenerCount("notification")).toBe(0);
    expect(client.listenerCount("error")).toBe(0);

    const stop = await bus.start(handlers);
    expect(client.listenAttempts).toBe(2);
    await stop();
    expect(client.releases).toBe(2);
  });

  test("chunks aggregate awareness into channel-safe notifications", async () => {
    const payloads: string[] = [];
    const client = new FakeAwarenessClient();
    const pool = {
      connect: async () => client,
      query: async (_queryText: string, values: unknown[]) => {
        const payload = values[1];
        if (typeof payload !== "string") throw new Error("The notify payload is missing");
        payloads.push(payload);
      },
    };
    const bus = new PostgresSpecAwarenessBus(pool);
    const aggregate = new awarenessProtocol.Awareness(new Y.Doc());
    aggregate.setLocalState(null);
    const sources: Array<{ doc: Y.Doc; awareness: awarenessProtocol.Awareness }> = [];
    for (let index = 0; index < 20; index += 1) {
      const doc = new Y.Doc();
      const awareness = new awarenessProtocol.Awareness(doc);
      awareness.setLocalState({ cursor: "x".repeat(500), index });
      awarenessProtocol.applyAwarenessUpdate(
        aggregate,
        awarenessProtocol.encodeAwarenessUpdate(awareness, [doc.clientID]),
        "test",
      );
      sources.push({ doc, awareness });
    }
    const update = awarenessProtocol.encodeAwarenessUpdate(aggregate, [
      ...aggregate.getStates().keys(),
    ]);

    await bus.publish("00000000-0000-4000-8000-000000000104", update);

    expect(payloads.length).toBeGreaterThan(1);
    const result = new awarenessProtocol.Awareness(new Y.Doc());
    result.setLocalState(null);
    for (const payload of payloads) {
      expect(Buffer.byteLength(payload, "utf8")).toBeLessThanOrEqual(
        SPEC_CHANNEL_PAYLOAD_MAX_BYTES,
      );
      const envelope = parseSpecChannelEnvelope(payload);
      if (envelope?.type !== "awareness") throw new Error("Expected an awareness envelope");
      awarenessProtocol.applyAwarenessUpdate(
        result,
        Buffer.from(envelope.update, "base64"),
        "test",
      );
    }
    expect(result.getStates().size).toBe(20);

    for (const source of sources) {
      source.awareness.destroy();
      source.doc.destroy();
    }
    aggregate.destroy();
    result.destroy();
  });

  test("publishes the participant epoch used to supersede peer sockets", async () => {
    const payloads: string[] = [];
    const pool = {
      connect: async () => new FakeAwarenessClient(),
      query: async (_queryText: string, values: unknown[]) => {
        const payload = values[1];
        if (typeof payload !== "string") throw new Error("The notify payload is missing");
        payloads.push(payload);
      },
    };
    const bus = new PostgresSpecAwarenessBus(pool);

    await bus.publishParticipantConnected(
      "00000000-0000-4000-8000-000000000104",
      "42",
      9_007_199_254_740_993n,
    );

    expect(payloads).toHaveLength(1);
    expect(parseSpecChannelEnvelope(payloads[0]!)).toEqual({
      type: "participant-connected",
      specId: "00000000-0000-4000-8000-000000000104",
      clientId: "42",
      epoch: "9007199254740993",
    });
  });
});

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
let livePool: Pool | null = DB_URL ? new Pool({ connectionString: DB_URL }) : null;
let liveDbReachable = false;

if (livePool) {
  liveDbReachable = await livePool
    .query("SELECT 1")
    .then(() => true)
    .catch(() => false);
}

describe("PostgresSpecParticipantStore", () => {
  const templateId = randomUUID();
  const specId = randomUUID();
  const firstUserId = `spec-participant-first-${randomUUID()}`;
  const secondUserId = `spec-participant-second-${randomUUID()}`;

  beforeAll(async () => {
    if (!liveDbReachable || !livePool) return;
    const now = new Date();
    await livePool.query(
      `INSERT INTO "user" (id, name, email, email_verified, created_at, updated_at)
       VALUES ($1, 'First member', $2, false, $3, $3),
              ($4, 'Second member', $5, false, $3, $3)`,
      [
        firstUserId,
        `${firstUserId}@example.test`,
        now,
        secondUserId,
        `${secondUserId}@example.test`,
      ],
    );
    await livePool.query(
      `INSERT INTO spec_template (id, name, layers, sections, stage_flags)
       VALUES ($1, 'Participant test', '[]', '[]', '{}')`,
      [templateId],
    );
    await livePool.query(
      `INSERT INTO spec (id, org_id, template_id, title, lifecycle)
       VALUES ($1, 'test-org', $2, 'Participant test', 'draft')`,
      [specId, templateId],
    );
  });

  afterAll(async () => {
    if (!livePool) return;
    if (liveDbReachable) {
      await livePool.query("DELETE FROM spec WHERE id = $1", [specId]);
      await livePool.query("DELETE FROM spec_template WHERE id = $1", [templateId]);
      await livePool.query('DELETE FROM "user" WHERE id = ANY($1)', [[firstUserId, secondUserId]]);
    }
    await livePool.end();
    livePool = null;
  });

  test.skipIf(!liveDbReachable)(
    "keeps a client id bound to its first user across replicas",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const participants = new PostgresSpecParticipantStore(drizzle(livePool, { schema }));

      await participants.connect(specId, "same-client", firstUserId);
      await participants.connect(specId, "same-client", firstUserId);
      await expect(participants.connect(specId, "same-client", secondUserId)).rejects.toThrow(
        "already bound to another user",
      );

      const result = await livePool.query(
        "SELECT user_id FROM spec_participant WHERE spec_id = $1 AND client_id = $2",
        [specId, "same-client"],
      );
      expect(result.rows).toEqual([{ user_id: firstUserId }]);
    },
  );

  test.skipIf(!liveDbReachable)(
    "stale heartbeat and disconnect ordering cannot clear the intended live epoch",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      let current = new Date("2026-08-10T12:00:00Z");
      const participants = new PostgresSpecParticipantStore(
        drizzle(livePool, { schema }),
        () => current,
        60_000,
      );
      const clientId = `reconnect-${randomUUID()}`;
      const oldEpoch = await participants.connect(specId, clientId, firstUserId);
      current = new Date(current.getTime() + 1_000);
      const newEpoch = await participants.connect(specId, clientId, firstUserId);

      await participants.disconnect(specId, clientId, newEpoch);
      await participants.renew(specId, clientId, oldEpoch);
      await participants.disconnect(specId, clientId, oldEpoch);
      current = new Date(current.getTime() + 1_000);
      const liveEpoch = await participants.connect(specId, clientId, firstUserId);
      await participants.renew(specId, clientId, oldEpoch);
      await participants.disconnect(specId, clientId, newEpoch);

      const result = await livePool.query(
        `SELECT connection_epoch, disconnected_at, lease_expires_at > $3 AS active
         FROM spec_participant WHERE spec_id = $1 AND client_id = $2`,
        [specId, clientId, current],
      );
      expect(newEpoch).toBe(oldEpoch + 1n);
      expect(liveEpoch).toBe(newEpoch + 1n);
      expect(result.rows).toEqual([
        { connection_epoch: liveEpoch.toString(), disconnected_at: null, active: true },
      ]);
    },
  );

  test.skipIf(!liveDbReachable)(
    "migration 0055 supports old SQL and explicit lease writers during rollout",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const client = await livePool.connect();
      const schemaName = `lease_contract_${randomUUID().replaceAll("-", "")}`;
      const quotedSchema = `"${schemaName}"`;
      const specId = randomUUID();
      const userId = `mixed-version-${randomUUID()}`;
      const initialTime = new Date("2026-08-10T12:00:00.000Z");
      const reconnectTime = new Date("2026-08-10T12:02:00.000Z");
      const currentTime = new Date("2026-08-10T12:04:00.000Z");
      try {
        await client.query(`CREATE SCHEMA ${quotedSchema}`);
        await client.query(`SET search_path TO ${quotedSchema}`);
        await client.query(
          `CREATE TABLE spec_participant (
             spec_id uuid NOT NULL,
             client_id text NOT NULL,
             user_id text,
             connection_epoch bigint DEFAULT 0 NOT NULL,
             connected_at timestamptz NOT NULL,
             disconnected_at timestamptz,
             lease_expires_at timestamptz NOT NULL,
             PRIMARY KEY (spec_id, client_id)
           )`,
        );
        await client.query(
          `INSERT INTO spec_participant
             (spec_id, client_id, user_id, connected_at, disconnected_at, lease_expires_at)
           VALUES ($1, 'existing-old', $2, $3, NULL, $3)`,
          [specId, userId, initialTime],
        );

        const migration = await readFile(
          new URL("../../../drizzle/0055_spec_participant_rollout_contract.sql", import.meta.url),
          "utf8",
        );
        for (const statement of migration
          .split("--> statement-breakpoint")
          .map((value) => value.trim())
          .filter(Boolean)) {
          await client.query(statement);
        }

        const oldConnect = async (clientId: string, connectedAt: Date): Promise<void> => {
          await client.query(
            `INSERT INTO spec_participant
               (spec_id, client_id, user_id, connected_at, disconnected_at)
             VALUES ($1, $2, $3, $4, NULL)
             ON CONFLICT (spec_id, client_id) DO UPDATE
             SET user_id = excluded.user_id,
                 connected_at = excluded.connected_at,
                 disconnected_at = NULL
             WHERE spec_participant.user_id IS NULL
                OR spec_participant.user_id = excluded.user_id`,
            [specId, clientId, userId, connectedAt],
          );
        };

        await oldConnect("old-insert", initialTime);
        const initialLegacyRows = await client.query<{ all_infinite: boolean }>(
          `SELECT bool_and(lease_expires_at = 'infinity'::timestamptz) AS all_infinite
             FROM spec_participant
            WHERE client_id IN ('existing-old', 'old-insert')`,
        );
        expect(initialLegacyRows.rows).toEqual([{ all_infinite: true }]);

        let now = initialTime;
        const participants = new PostgresSpecParticipantStore(
          drizzle(client, { schema }),
          () => now,
          60_000,
        );
        const firstNewEpoch = await participants.connect(specId, "mixed-client", userId);
        await oldConnect("mixed-client", reconnectTime);
        const oldUpsert = await client.query<{ connection_epoch: string; lease_is_infinite: boolean }>(
          `SELECT connection_epoch,
                  lease_expires_at = 'infinity'::timestamptz AS lease_is_infinite
             FROM spec_participant
            WHERE spec_id = $1 AND client_id = 'mixed-client'`,
          [specId],
        );
        expect(oldUpsert.rows).toEqual([
          { connection_epoch: firstNewEpoch.toString(), lease_is_infinite: true },
        ]);

        now = currentTime;
        const currentEpoch = await participants.connect(specId, "mixed-client", userId);
        expect(currentEpoch).toBe(firstNewEpoch + 1n);
        expect(await participants.renew(specId, "mixed-client", currentEpoch)).toBe(true);
        const newWriter = await client.query<{
          connection_epoch: string;
          finite_expiry: boolean;
        }>(
          `SELECT connection_epoch,
                  lease_expires_at = $2::timestamptz AS finite_expiry
             FROM spec_participant
            WHERE spec_id = $1 AND client_id = 'mixed-client'`,
          [specId, new Date(currentTime.getTime() + 60_000)],
        );
        expect(newWriter.rows).toEqual([
          { connection_epoch: currentEpoch.toString(), finite_expiry: true },
        ]);
      } finally {
        await client.query("SET search_path TO public");
        await client.query(`DROP SCHEMA IF EXISTS ${quotedSchema} CASCADE`);
        client.release();
      }
    },
  );
});
