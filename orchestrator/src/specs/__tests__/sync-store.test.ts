import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool } from "pg";

import * as schema from "../../db/schema.ts";
import { PostgresSpecAwarenessBus } from "../sync-store.ts";
import { PostgresSpecParticipantStore } from "../sync-store.ts";

describe("PostgresSpecAwarenessBus", () => {
  test("releases the client and permits a retry when LISTEN fails", async () => {
    let listenAttempts = 0;
    let releases = 0;
    let attachedListener: ((message: { channel: string; payload?: string }) => void) | null = null;
    const client = {
      on: (
        _event: "notification",
        listener: (message: { channel: string; payload?: string }) => void,
      ) => {
        attachedListener = listener;
      },
      off: (
        _event: "notification",
        listener: (message: { channel: string; payload?: string }) => void,
      ) => {
        if (attachedListener === listener) attachedListener = null;
      },
      query: async () => {
        listenAttempts += 1;
        if (listenAttempts === 1) throw new Error("LISTEN failed");
      },
      release: () => {
        releases += 1;
      },
    };
    const pool = {
      connect: async () => client,
      query: async () => {},
    };
    const bus = new PostgresSpecAwarenessBus(pool);
    const handlers = { update: () => {}, query: () => {} };

    await expect(bus.start(handlers)).rejects.toThrow("LISTEN failed");
    expect(releases).toBe(1);
    expect(attachedListener).toBeNull();

    const stop = await bus.start(handlers);
    expect(listenAttempts).toBe(2);
    await stop();
    expect(releases).toBe(2);
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
});
