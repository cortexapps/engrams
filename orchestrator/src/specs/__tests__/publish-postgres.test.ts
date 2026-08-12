import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool } from "pg";
import pino from "pino";

import { makeArtifactStore } from "../../db/artifacts.ts";
import * as schema from "../../db/schema.ts";
import type { SpecRailMetadata, SpecRailStore } from "../../routes/spec-rail.ts";
import { PostgresSpecCheckpointStore } from "../checkpoints.ts";
import type { CreateCheckpointOptions, SpecCheckpointRecord } from "../checkpoints.ts";
import type { LoadedSpecDocument } from "../doc-service.ts";
import type { GapCheckRun, GapCheckStatus } from "../gap-check.ts";
import { makeSpecPublishArtifactPublisher } from "../publish-artifact.ts";
import { runSpecPublishTick, type SpecTicketizeHandoff } from "../publish-scanner.ts";
import {
  PostgresSpecPublishStore,
  SpecPublishError,
  SpecPublishService,
  type SpecPublishRecord,
} from "../publish.ts";
import { schema as documentSchema, SPEC_FRAGMENT_NAME } from "@engrams/spec-document";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import * as Y from "yjs";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const pool: Pool | null = DB_URL ? new Pool({ connectionString: DB_URL, max: 4 }) : null;
let reachable = false;

if (pool) {
  reachable = await pool
    .query("SELECT 1")
    .then(() => true)
    .catch(() => false);
}

const NOW = new Date("2026-08-12T15:04:00.000Z");
const log = pino({ enabled: false });

const TEMPLATE_SECTIONS: schema.SpecTemplateSection[] = [
  {
    key: "requirements",
    title: "Requirements",
    layerKey: "intent",
    guidance: "List the requirements.",
    doneCriteria: [],
    required: true,
    allowNa: false,
  },
  {
    key: "data",
    title: "Data model",
    layerKey: "contract",
    guidance: "Give the data model.",
    doneCriteria: [],
    required: true,
    allowNa: true,
  },
];

function specDocument(): LoadedSpecDocument {
  const document = documentSchema.nodes.doc!.create(null, [
    documentSchema.nodes.section!.create({ id: "sec-req", templateSectionKey: "requirements" }, [
      documentSchema.nodes.sectionHeading!.create(null, documentSchema.text("Requirements")),
      documentSchema.nodes.paragraph!.create(null, [
        documentSchema.text("- R1: an org caps its sandboxes"),
      ]),
    ]),
    documentSchema.nodes.section!.create({ id: "sec-data", templateSectionKey: "data" }, [
      documentSchema.nodes.sectionHeading!.create(null, documentSchema.text("Data model")),
      documentSchema.nodes.paragraph!.create(null, [
        documentSchema.text("A counter row per org."),
      ]),
    ]),
  ]);
  const ydoc = new Y.Doc();
  prosemirrorToYXmlFragment(document, ydoc.getXmlFragment(SPEC_FRAGMENT_NAME));
  return { doc: ydoc, lastAppliedSeq: 4n, semanticDocSeq: 4n };
}

class MemoryRailStore implements SpecRailStore {
  states = new Map<string, { state: "empty" | "drafted" | "confirmed" | "n/a"; naReason: string | null }>(
    [
      ["sec-req", { state: "confirmed", naReason: null }],
      ["sec-data", { state: "confirmed", naReason: null }],
    ],
  );

  async readMetadata(): Promise<SpecRailMetadata | null> {
    return {
      lifecycle: "draft",
      layers: [
        { key: "intent", title: "Intent" },
        { key: "contract", title: "Contract" },
      ],
      sections: TEMPLATE_SECTIONS,
      states: this.states,
      openQuestionCounts: new Map(),
    };
  }
}

const freshGapCheck = {
  async status(): Promise<GapCheckStatus> {
    return { run: null, currentSemanticDocSeq: 4n, stale: false };
  },
  async run(): Promise<GapCheckRun> {
    throw new Error("the gap check is fresh, so it must not run");
  },
};

/**
 * The checkpoint content the compaction would produce. The real
 * PostgresSpecCheckpointStore does the insert, so the ON CONFLICT rule that
 * makes the pin exactly-once is the one under test.
 */
class TestCheckpoints {
  compactions = 0;

  constructor(
    private readonly store: PostgresSpecCheckpointStore,
    private readonly specId: string,
  ) {}

  async createCheckpoint(
    specId: string,
    options: CreateCheckpointOptions,
  ): Promise<SpecCheckpointRecord> {
    this.compactions += 1;
    const checkpoint: SpecCheckpointRecord = {
      id: options.id ?? randomUUID(),
      specId,
      state: new Uint8Array([1, 2, 3]),
      stateVector: new Uint8Array([4]),
      renderedMarkdown: `# Org sandbox quotas\n\nCompaction ${this.compactions}\n`,
      docSeq: 4n,
      label: options.label,
      authorUserId: options.authorUserId ?? null,
      reason: options.reason,
      createdAt: NOW,
    };
    await this.store.insertCheckpoint(checkpoint);
    const stored = await this.store.readCheckpoint(this.specId, checkpoint.id);
    return stored ?? checkpoint;
  }
}

class RecordingTicketize implements SpecTicketizeHandoff {
  readonly starts: string[] = [];

  async start(input: { promptId: string }): Promise<void> {
    this.starts.push(input.promptId);
  }
}

describe("spec publish with live Postgres", () => {
  const templateId = randomUUID();
  const specId = randomUUID();
  const otherSpecId = randomUUID();
  const sessionId = randomUUID();
  const owner = `spec-publish-owner-${randomUUID()}`;
  const member = `spec-publish-member-${randomUUID()}`;

  beforeAll(async () => {
    if (!reachable || !pool) return;
    await pool.query(
      `INSERT INTO "user" (id, name, email, email_verified, created_at, updated_at)
       VALUES ($1, 'Owner', $1 || '@example.test', false, now(), now()),
              ($2, 'Member', $2 || '@example.test', false, now(), now())`,
      [owner, member],
    );
    await pool.query(
      `INSERT INTO spec_template (id, name, layers, sections, stage_flags)
       VALUES ($1, 'Publish test template', '[]', $2, '{"alternatives":"on","talkItThrough":"suggested","gapCheck":"on"}')`,
      [templateId, JSON.stringify(TEMPLATE_SECTIONS)],
    );
    for (const id of [specId, otherSpecId]) {
      await pool.query(
        `INSERT INTO spec (id, org_id, owner_user_id, session_id, template_id, title, lifecycle)
         VALUES ($1, 'test-org', $2, $3, $4, 'Org sandbox quotas', 'draft')`,
        [id, owner, sessionId, templateId],
      );
    }
  });

  afterAll(async () => {
    if (!reachable || !pool) return;
    await pool.query("DELETE FROM spec WHERE id = ANY($1::uuid[])", [[specId, otherSpecId]]);
    await pool.query("DELETE FROM spec_template WHERE id = $1", [templateId]);
    await pool.query('DELETE FROM artifact WHERE owner_user_id = ANY($1::text[])', [
      [owner, member],
    ]);
    await pool.query('DELETE FROM "user" WHERE id = ANY($1::text[])', [[owner, member]]);
    await pool.end();
  });

  function service(store: PostgresSpecPublishStore, railStore = new MemoryRailStore()) {
    return new SpecPublishService({
      store,
      railStore,
      documents: { syncFromLog: async () => specDocument() },
      gapCheck: freshGapCheck,
      now: () => NOW,
    });
  }

  function scannerDeps(store: PostgresSpecPublishStore, targetSpecId: string) {
    const checkpointStore = new PostgresSpecCheckpointStore(pool!);
    const checkpoints = new TestCheckpoints(checkpointStore, targetSpecId);
    const artifacts = makeSpecPublishArtifactPublisher({
      store: makeArtifactStore(drizzle(pool!, { schema })),
      pull: {
        async createArtifactFromPath() {
          return {
            artifactId: randomUUID(),
            mediaType: "text/markdown",
            sizeBytes: 64,
          };
        },
      },
      guest: {
        async writeFile() {
          return { path: "staged", sizeBytes: 64n, sha256: "" };
        },
        readFile() {
          throw new Error("the publish path never reads a guest file");
        },
      },
    });
    const ticketize = new RecordingTicketize();
    return {
      deps: {
        store,
        checkpoints,
        checkpointStore,
        artifacts,
        ticketize,
        config: { batchSize: 10, retryDelayMs: 0 },
        // One second after the request, so the claim is due on the first sweep.
        now: () => new Date(NOW.getTime() + 1_000),
        log,
      },
      checkpoints,
      ticketize,
    };
  }

  test.skipIf(!reachable)("a replayed request records one publish row", async () => {
    const store = new PostgresSpecPublishStore(pool!);
    const publish = service(store);

    const first = await publish.requestPublish({
      specId,
      actorUserId: owner,
      actionId: randomUUID(),
      acknowledgeOpenQuestions: false,
      runGapCheck: false,
    });
    const second = await publish.requestPublish({
      specId,
      actorUserId: owner,
      actionId: randomUUID(),
      acknowledgeOpenQuestions: false,
      runGapCheck: false,
    });

    expect(first.created).toBe(true);
    expect(second.created).toBe(false);
    expect(second.publish.checkpointId).toBe(first.publish.checkpointId);
    expect(second.publish.artifactId).toBe(first.publish.artifactId);
    const rows = await pool!.query<{ count: number }>(
      "SELECT count(*)::int AS count FROM spec_publish WHERE spec_id = $1",
      [specId],
    );
    expect(rows.rows[0]!.count).toBe(1);
  });

  test.skipIf(!reachable)(
    "publishing pins exactly one checkpoint and one artifact version",
    async () => {
      const store = new PostgresSpecPublishStore(pool!);
      const first = scannerDeps(store, specId);

      // Drive the machine four times. Every step after the first is a replay.
      await runSpecPublishTick(first.deps);
      await runSpecPublishTick(first.deps);
      const second = scannerDeps(store, specId);
      await runSpecPublishTick(second.deps);
      await runSpecPublishTick(second.deps);

      const record = await store.readPublish(specId);
      expect(record?.state).toBe("complete");
      expect(record?.artifactVersion).toBe(1);

      const spec = await pool!.query<{
        lifecycle: string;
        published_checkpoint_id: string | null;
        published_by: string | null;
        published_at: Date | null;
      }>(
        `SELECT lifecycle, published_checkpoint_id, published_by, published_at
           FROM spec WHERE id = $1`,
        [specId],
      );
      expect(spec.rows[0]!.lifecycle).toBe("published");
      expect(spec.rows[0]!.published_checkpoint_id).toBe(record!.checkpointId);
      expect(spec.rows[0]!.published_by).toBe(owner);
      expect(spec.rows[0]!.published_at).not.toBeNull();

      const checkpoints = await pool!.query<{ count: number }>(
        "SELECT count(*)::int AS count FROM spec_checkpoint WHERE spec_id = $1 AND reason = 'publish'",
        [specId],
      );
      expect(checkpoints.rows[0]!.count).toBe(1);

      const versions = await pool!.query<{ count: number }>(
        "SELECT count(*)::int AS count FROM artifact_version WHERE artifact_id = $1",
        [record!.artifactId],
      );
      expect(versions.rows[0]!.count).toBe(1);
      const artifact = await pool!.query<{ current_version: number; owner_user_id: string | null }>(
        "SELECT current_version, owner_user_id FROM artifact WHERE id = $1",
        [record!.artifactId],
      );
      expect(artifact.rows[0]!.current_version).toBe(1);
      expect(artifact.rows[0]!.owner_user_id).toBe(owner);

      // The ticketize hand-off carries the stable prompt id, once.
      expect(first.ticketize.starts.length + second.ticketize.starts.length).toBe(1);
    },
  );

  test.skipIf(!reachable)("a non-owner org member cannot publish (R37)", async () => {
    const store = new PostgresSpecPublishStore(pool!);
    const publish = service(store);

    let refusal: SpecPublishError | null = null;
    try {
      await publish.requestPublish({
        specId: otherSpecId,
        actorUserId: member,
        actionId: randomUUID(),
        acknowledgeOpenQuestions: false,
        runGapCheck: false,
      });
    } catch (error) {
      if (!(error instanceof SpecPublishError)) throw error;
      refusal = error;
    }

    expect(refusal?.code).toBe("not_owner");
    expect(refusal?.status?.canPublish).toBe(false);
    // The member still reads the gate, because a spec is org-visible (D12).
    expect(refusal?.status?.gate.ready).toBe(true);
    const rows = await pool!.query<{ count: number }>(
      "SELECT count(*)::int AS count FROM spec_publish WHERE spec_id = $1",
      [otherSpecId],
    );
    expect(rows.rows[0]!.count).toBe(0);
    const spec = await pool!.query<{ lifecycle: string }>(
      "SELECT lifecycle FROM spec WHERE id = $1",
      [otherSpecId],
    );
    expect(spec.rows[0]!.lifecycle).toBe("draft");
  });

  test.skipIf(!reachable)("a claim is exclusive: two drivers, one claimed row", async () => {
    const store = new PostgresSpecPublishStore(pool!);
    const now = new Date();
    const record: SpecPublishRecord = {
      specId: otherSpecId,
      sessionId,
      checkpointId: randomUUID(),
      artifactId: randomUUID(),
      artifactVersion: null,
      state: "requested",
      requestedBy: owner,
      requestedAt: now,
      acknowledgedQuestionCount: 0,
      acknowledgedQuestionIds: [],
      gapCheckRunId: null,
      attempts: 0,
      nextAttemptAt: now,
      lastError: null,
      pinnedAt: null,
      completedAt: null,
    };
    await store.insertRequest(record);

    const [a, b] = await Promise.all([
      store.claimDue({ now, retryAt: new Date(now.getTime() + 60_000), limit: 10 }),
      store.claimDue({ now, retryAt: new Date(now.getTime() + 60_000), limit: 10 }),
    ]);

    const claimed = [...a, ...b].filter((row) => row.specId === otherSpecId);
    expect(claimed).toHaveLength(1);
    expect(claimed[0]!.specTitle).toBe("Org sandbox quotas");
    expect(claimed[0]!.ownerUserId).toBe(owner);

    // markPinned advances once, whichever driver calls it again.
    const pinInput = {
      specId: otherSpecId,
      checkpointId: record.checkpointId,
      publishedBy: owner,
      at: now,
    };
    const checkpointStore = new PostgresSpecCheckpointStore(pool!);
    await checkpointStore.insertCheckpoint({
      id: record.checkpointId,
      specId: otherSpecId,
      state: new Uint8Array([1]),
      stateVector: new Uint8Array([2]),
      renderedMarkdown: "# pinned\n",
      docSeq: 4n,
      label: "Published version",
      authorUserId: owner,
      reason: "publish",
      createdAt: now,
    });
    expect(await store.markPinned(pinInput)).toBe(true);
    expect(await store.markPinned(pinInput)).toBe(false);
    const spec = await pool!.query<{ lifecycle: string }>(
      "SELECT lifecycle FROM spec WHERE id = $1",
      [otherSpecId],
    );
    expect(spec.rows[0]!.lifecycle).toBe("published");
  });
});
