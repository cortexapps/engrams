import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { EventEmitter } from "node:events";
import { readFile } from "node:fs/promises";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool, type PoolClient } from "pg";
import * as awarenessProtocol from "y-protocols/awareness";
import * as Y from "yjs";

import * as schema from "../../db/schema.ts";
import { PostgresSpecAwarenessBus } from "../sync-store.ts";
import { PostgresSpecParticipantStore } from "../sync-store.ts";
import {
  parseSpecChannelEnvelope,
  PostgresSpecDocumentStore,
  SPEC_CHANNEL_PAYLOAD_MAX_BYTES,
} from "../doc-service.ts";

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

/** Run one drizzle migration file against the client's current search path. */
async function applyMigration(client: PoolClient, fileName: string): Promise<void> {
  const migration = await readFile(
    new URL(`../../../drizzle/${fileName}`, import.meta.url),
    "utf8",
  );
  for (const statement of migration
    .split("--> statement-breakpoint")
    .map((value) => value.trim())
    .filter(Boolean)) {
    await client.query(statement);
  }
}

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
      let documentPool: Pool | null = null;
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
          `CREATE TABLE spec (
             id uuid PRIMARY KEY,
             lifecycle text NOT NULL,
             current_doc_seq bigint DEFAULT 0 NOT NULL,
             current_semantic_doc_seq bigint DEFAULT 0 NOT NULL,
             updated_at timestamptz NOT NULL
           )`,
        );
        await client.query(
          `CREATE TABLE spec_update_log (
             spec_id uuid NOT NULL,
             seq bigint NOT NULL,
             semantic_doc_seq bigint NOT NULL,
             update bytea NOT NULL,
             client_id text,
             PRIMARY KEY (spec_id, seq)
           )`,
        );
        await client.query(
          `INSERT INTO spec (id, lifecycle, current_doc_seq, updated_at)
           VALUES ($1, 'draft', 0, $2)`,
          [specId, initialTime],
        );
        await client.query(
          `INSERT INTO spec_participant
             (spec_id, client_id, user_id, connected_at, disconnected_at, lease_expires_at)
           VALUES ($1, 'existing-old', $2, $3, NULL, $3)`,
          [specId, userId, initialTime],
        );

        await applyMigration(client, "0055_spec_participant_rollout_contract.sql");

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
        const oldDisconnect = async (clientId: string, disconnectedAt: Date): Promise<void> => {
          await client.query(
            `UPDATE spec_participant
                SET disconnected_at = $3
              WHERE spec_id = $1 AND client_id = $2`,
            [specId, clientId, disconnectedAt],
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
        const oldUpsert = await client.query<{
          connection_epoch: string;
          connected_at: Date;
          disconnected_at: Date | null;
          lease_expires_at: Date;
        }>(
          `SELECT connection_epoch, connected_at, disconnected_at, lease_expires_at
             FROM spec_participant
            WHERE spec_id = $1 AND client_id = 'mixed-client'`,
          [specId],
        );
        expect(oldUpsert.rows).toEqual([
          {
            connection_epoch: firstNewEpoch.toString(),
            connected_at: initialTime,
            disconnected_at: null,
            lease_expires_at: new Date(initialTime.getTime() + 60_000),
          },
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

        const equalExpiryClientId = "current-disconnect-at-expiry";
        const equalExpiryConnectedAt = new Date("2026-08-10T12:06:00.000Z");
        const equalExpiryDisconnectAt = new Date(equalExpiryConnectedAt.getTime() + 60_000);
        now = equalExpiryConnectedAt;
        const equalExpiryEpoch = await participants.connect(specId, equalExpiryClientId, userId);
        now = equalExpiryDisconnectAt;
        await participants.disconnect(specId, equalExpiryClientId, equalExpiryEpoch);
        const equalExpiryDisconnect = await client.query<{
          connection_epoch: string;
          disconnected_at: Date | null;
          lease_is_tombstone: boolean;
        }>(
          `SELECT connection_epoch,
                  disconnected_at,
                  lease_expires_at = '-infinity'::timestamptz AS lease_is_tombstone
             FROM spec_participant
            WHERE spec_id = $1 AND client_id = $2`,
          [specId, equalExpiryClientId],
        );
        expect(equalExpiryDisconnect.rows).toEqual([
          {
            connection_epoch: equalExpiryEpoch.toString(),
            disconnected_at: equalExpiryDisconnectAt,
            lease_is_tombstone: true,
          },
        ]);

        now = new Date(equalExpiryDisconnectAt.getTime() + 1_000);
        const reconnectedEpoch = await participants.connect(specId, equalExpiryClientId, userId);
        const reconnected = await client.query<{
          connection_epoch: string;
          disconnected_at: Date | null;
          lease_expires_at: Date;
        }>(
          `SELECT connection_epoch, disconnected_at, lease_expires_at
             FROM spec_participant
            WHERE spec_id = $1 AND client_id = $2`,
          [specId, equalExpiryClientId],
        );
        expect(reconnectedEpoch).toBe(equalExpiryEpoch + 1n);
        expect(reconnected.rows).toEqual([
          {
            connection_epoch: reconnectedEpoch.toString(),
            disconnected_at: null,
            lease_expires_at: new Date(now.getTime() + 60_000),
          },
        ]);

        const protectedClientId = "legacy-connect-new-epoch";
        await oldConnect(protectedClientId, initialTime);
        now = reconnectTime;
        const protectedEpoch = await participants.connect(specId, protectedClientId, userId);
        await oldDisconnect(protectedClientId, currentTime);

        now = currentTime;
        expect(await participants.renew(specId, protectedClientId, protectedEpoch)).toBe(true);
        documentPool = new Pool({
          connectionString: DB_URL,
          options: `-c search_path=${schemaName}`,
        });
        const documents = new PostgresSpecDocumentStore(documentPool);
        expect(
          await documents.insertUpdateIfLatest(
            specId,
            0n,
            new Uint8Array([1]),
            protectedClientId,
            { sections: [], semanticChanged: false },
            protectedEpoch,
          ),
        ).toEqual({ seq: 1n, semanticDocSeq: 0n });
        const protectedRow = await client.query<{
          connection_epoch: string;
          disconnected_at: Date | null;
          lease_expires_at: Date;
        }>(
          `SELECT connection_epoch, disconnected_at, lease_expires_at
             FROM spec_participant
            WHERE spec_id = $1 AND client_id = $2`,
          [specId, protectedClientId],
        );
        expect(protectedEpoch).toBe(1n);
        expect(protectedRow.rows).toEqual([
          {
            connection_epoch: protectedEpoch.toString(),
            disconnected_at: null,
            lease_expires_at: new Date(currentTime.getTime() + 60_000),
          },
        ]);

        const legacyTakeoverClientId = "legacy-reconnect-disconnected-current";
        const legacyReconnectTime = new Date(currentTime.getTime() + 1_000);
        const legacyDisconnectTime = new Date(currentTime.getTime() + 2_000);
        const disconnectedCurrentEpoch = await participants.connect(
          specId,
          legacyTakeoverClientId,
          userId,
        );
        await participants.disconnect(specId, legacyTakeoverClientId, disconnectedCurrentEpoch);
        await oldConnect(legacyTakeoverClientId, legacyReconnectTime);
        const legacyTakeover = await client.query<{
          connection_epoch: string;
          connected_at: Date;
          disconnected_at: Date | null;
          lease_is_infinite: boolean;
        }>(
          `SELECT connection_epoch,
                  connected_at,
                  disconnected_at,
                  lease_expires_at = 'infinity'::timestamptz AS lease_is_infinite
             FROM spec_participant
            WHERE spec_id = $1 AND client_id = $2`,
          [specId, legacyTakeoverClientId],
        );
        expect(legacyTakeover.rows).toEqual([
          {
            connection_epoch: "0",
            connected_at: legacyReconnectTime,
            disconnected_at: null,
            lease_is_infinite: true,
          },
        ]);

        await oldDisconnect(legacyTakeoverClientId, legacyDisconnectTime);
        const closedLegacyTakeover = await client.query<{
          connection_epoch: string;
          disconnected_at: Date | null;
          lease_is_infinite: boolean;
        }>(
          `SELECT connection_epoch,
                  disconnected_at,
                  lease_expires_at = 'infinity'::timestamptz AS lease_is_infinite
             FROM spec_participant
            WHERE spec_id = $1 AND client_id = $2`,
          [specId, legacyTakeoverClientId],
        );
        expect(closedLegacyTakeover.rows).toEqual([
          {
            connection_epoch: "0",
            disconnected_at: legacyDisconnectTime,
            lease_is_infinite: true,
          },
        ]);
      } finally {
        await documentPool?.end();
        await client.query("SET search_path TO public");
        await client.query(`DROP SCHEMA IF EXISTS ${quotedSchema} CASCADE`);
        client.release();
      }
    },
  );

  test.skipIf(!liveDbReachable)(
    "migration 0064 removes the rollout contract and keeps the lease writers",
    async () => {
      if (!livePool) throw new Error("The live Postgres pool is not available");
      const client = await livePool.connect();
      const schemaName = `lease_contract_end_${randomUUID().replaceAll("-", "")}`;
      const quotedSchema = `"${schemaName}"`;
      const specId = randomUUID();
      const userId = `contract-end-${randomUUID()}`;
      const initialTime = new Date("2026-08-10T12:00:00.000Z");
      const disconnectTime = new Date("2026-08-10T12:01:00.000Z");
      const currentTime = new Date("2026-08-10T12:05:00.000Z");
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
        // A row that an old pod left connected before the expand migration.
        await client.query(
          `INSERT INTO spec_participant
             (spec_id, client_id, user_id, connected_at, disconnected_at, lease_expires_at)
           VALUES ($1, 'legacy-connected', $2, $3, NULL, $3)`,
          [specId, userId, initialTime],
        );

        await applyMigration(client, "0055_spec_participant_rollout_contract.sql");

        // Old-pod writes that the contract served: an insert without a lease,
        // and a disconnect without an epoch predicate.
        for (const clientId of ["legacy-inserted", "legacy-disconnected"]) {
          await client.query(
            `INSERT INTO spec_participant
               (spec_id, client_id, user_id, connected_at, disconnected_at)
             VALUES ($1, $2, $3, $4, NULL)`,
            [specId, clientId, userId, initialTime],
          );
        }
        await client.query(
          `UPDATE spec_participant
              SET disconnected_at = $3
            WHERE spec_id = $1 AND client_id = $2`,
          [specId, "legacy-disconnected", disconnectTime],
        );
        const beforeContract = await client.query<{ infinite_leases: string }>(
          `SELECT count(*)::text AS infinite_leases
             FROM spec_participant
            WHERE connection_epoch = 0 AND lease_expires_at = 'infinity'::timestamptz`,
        );
        expect(beforeContract.rows).toEqual([{ infinite_leases: "3" }]);

        // A current writer holds a finite lease across the contract migration.
        let now = initialTime;
        const participants = new PostgresSpecParticipantStore(
          drizzle(client, { schema }),
          () => now,
          60_000,
        );
        const survivingEpoch = await participants.connect(specId, "current-client", userId);

        await applyMigration(client, "0064_spec_participant_lease_contract.sql");

        // The compatibility contract is gone: no default, no trigger, no function.
        const columnDefault = await client.query<{ column_default: string | null }>(
          `SELECT column_default
             FROM information_schema.columns
            WHERE table_schema = $1
              AND table_name = 'spec_participant'
              AND column_name = 'lease_expires_at'`,
          [schemaName],
        );
        expect(columnDefault.rows).toEqual([{ column_default: null }]);
        const leftovers = await client.query<{ triggers: string; functions: string }>(
          `SELECT (SELECT count(*)::text
                     FROM pg_trigger t
                     JOIN pg_class c ON c.oid = t.tgrelid
                     JOIN pg_namespace n ON n.oid = c.relnamespace
                    WHERE n.nspname = $1
                      AND t.tgname = 'spec_participant_legacy_lease_compat') AS triggers,
                  (SELECT count(*)::text
                     FROM pg_proc p
                     JOIN pg_namespace n ON n.oid = p.pronamespace
                    WHERE n.nspname = $1
                      AND p.proname = 'spec_participant_legacy_lease_compat') AS functions`,
          [schemaName],
        );
        expect(leftovers.rows).toEqual([{ triggers: "0", functions: "0" }]);

        // Without the default, an old-pod insert can no longer land.
        await expect(
          client.query(
            `INSERT INTO spec_participant
               (spec_id, client_id, user_id, connected_at, disconnected_at)
             VALUES ($1, 'post-contract-legacy', $2, $3, NULL)`,
            [specId, userId, currentTime],
          ),
        ).rejects.toThrow('null value in column "lease_expires_at"');

        // Every epoch-zero infinite lease is expired, so stale presence is gone.
        const expired = await client.query<{ client_id: string; lease_is_tombstone: boolean }>(
          `SELECT client_id, lease_expires_at = '-infinity'::timestamptz AS lease_is_tombstone
             FROM spec_participant
            WHERE connection_epoch = 0
            ORDER BY client_id`,
        );
        expect(expired.rows).toEqual([
          { client_id: "legacy-connected", lease_is_tombstone: true },
          { client_id: "legacy-disconnected", lease_is_tombstone: true },
          { client_id: "legacy-inserted", lease_is_tombstone: true },
        ]);
        const stillLive = await client.query<{ live: string }>(
          `SELECT count(*)::text AS live
             FROM spec_participant
            WHERE lease_expires_at > $1::timestamptz`,
          [currentTime],
        );
        expect(stillLive.rows).toEqual([{ live: "0" }]);

        // The contract migration leaves the current writer's row alone.
        const survivor = await client.query<{
          connection_epoch: string;
          lease_expires_at: Date;
        }>(
          `SELECT connection_epoch, lease_expires_at
             FROM spec_participant
            WHERE spec_id = $1 AND client_id = 'current-client'`,
          [specId],
        );
        expect(survivor.rows).toEqual([
          {
            connection_epoch: survivingEpoch.toString(),
            lease_expires_at: new Date(initialTime.getTime() + 60_000),
          },
        ]);

        // Connect, renew, and disconnect still write finite lease values.
        now = currentTime;
        const reconnectedEpoch = await participants.connect(specId, "current-client", userId);
        expect(reconnectedEpoch).toBe(survivingEpoch + 1n);
        const connected = await client.query<{
          connection_epoch: string;
          disconnected_at: Date | null;
          lease_expires_at: Date;
        }>(
          `SELECT connection_epoch, disconnected_at, lease_expires_at
             FROM spec_participant
            WHERE spec_id = $1 AND client_id = 'current-client'`,
          [specId],
        );
        expect(connected.rows).toEqual([
          {
            connection_epoch: reconnectedEpoch.toString(),
            disconnected_at: null,
            lease_expires_at: new Date(currentTime.getTime() + 60_000),
          },
        ]);

        const renewTime = new Date(currentTime.getTime() + 30_000);
        now = renewTime;
        expect(await participants.renew(specId, "current-client", reconnectedEpoch)).toBe(true);
        const renewed = await client.query<{ lease_expires_at: Date }>(
          `SELECT lease_expires_at
             FROM spec_participant
            WHERE spec_id = $1 AND client_id = 'current-client'`,
          [specId],
        );
        expect(renewed.rows).toEqual([
          { lease_expires_at: new Date(renewTime.getTime() + 60_000) },
        ]);

        const disconnectedAt = new Date(renewTime.getTime() + 5_000);
        now = disconnectedAt;
        await participants.disconnect(specId, "current-client", reconnectedEpoch);
        const closed = await client.query<{
          disconnected_at: Date | null;
          lease_is_tombstone: boolean;
        }>(
          `SELECT disconnected_at, lease_expires_at = '-infinity'::timestamptz AS lease_is_tombstone
             FROM spec_participant
            WHERE spec_id = $1 AND client_id = 'current-client'`,
          [specId],
        );
        expect(closed.rows).toEqual([
          { disconnected_at: disconnectedAt, lease_is_tombstone: true },
        ]);
      } finally {
        await client.query("SET search_path TO public");
        await client.query(`DROP SCHEMA IF EXISTS ${quotedSchema} CASCADE`);
        client.release();
      }
    },
  );
});
