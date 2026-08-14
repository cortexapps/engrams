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
import { makeSpecPublishArtifactPublisher } from "../publish-artifact.ts";
import {
  runSpecPublishTick,
  ticketizePromptId,
  type SpecTicketizeHandoff,
} from "../publish-scanner.ts";
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
  states = new Map<string, { state: "open" | "proposed" | "settled" | "n/a"; naReason: string | null }>(
    [
      ["sec-req", { state: "settled", naReason: null }],
      ["sec-data", { state: "settled", naReason: null }],
    ],
  );

  async readMetadata(): Promise<SpecRailMetadata | null> {
    return {
      phase: "drafting",
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

/** The compaction the pin freezes. The real pin SQL writes the checkpoint. */
class TestDocuments {
  compactions = 0;
  semanticDocSeq = 4n;

  async compact(specId: string) {
    this.compactions += 1;
    void specId;
    return {
      state: new Uint8Array([1, 2, 3]),
      stateVector: new Uint8Array([4]),
      renderedMarkdown: `# Org sandbox quotas\n\nCompaction ${this.compactions}\n`,
      coveredSeq: 4n,
      coveredSemanticDocSeq: this.semanticDocSeq,
    };
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
  /** Its own spec, because this scenario publishes the row it starts. */
  const raceSpecId = randomUUID();
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
      `INSERT INTO spec_template (id, name, layers, sections)
       VALUES ($1, 'Publish test template', '[]', $2)`,
      [templateId, JSON.stringify(TEMPLATE_SECTIONS)],
    );
    for (const id of [specId, otherSpecId, raceSpecId]) {
      // The revision matches the fixture document, because the pin transaction
      // refuses to freeze a revision the spec row has already moved past.
      await pool.query(
        `INSERT INTO spec (id, org_id, owner_user_id, session_id, template_id, title,
                           phase, current_doc_seq, current_semantic_doc_seq)
         VALUES ($1, 'test-org', $2, $3, $4, 'Org sandbox quotas', 'drafting', 4, 4)`,
        [id, owner, sessionId, templateId],
      );
      // Section states exist, but the publish confirmation does not read them.
      await pool.query(
        `INSERT INTO spec_section_state (spec_id, section_id, state, na_reason)
         VALUES ($1, 'sec-req', 'settled', NULL), ($1, 'sec-data', 'settled', NULL)`,
        [id],
      );
    }
  });

  afterAll(async () => {
    if (!reachable || !pool) return;
    await pool.query("DELETE FROM spec WHERE id = ANY($1::uuid[])", [
      [specId, otherSpecId, raceSpecId],
    ]);
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
      now: () => NOW,
    });
  }

  function scannerDeps(store: PostgresSpecPublishStore, railStore = new MemoryRailStore()) {
    const checkpointStore = new PostgresSpecCheckpointStore(pool!);
    const documents = new TestDocuments();
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
        documents,
        // The real gate service, so the pin re-check is the production one.
        gate: service(store, railStore),
        checkpointStore,
        artifacts,
        ticketize,
        config: { batchSize: 10, retryDelayMs: 0 },
        // One second after the request, so the claim is due on the first sweep.
        now: () => new Date(NOW.getTime() + 1_000),
        log,
      },
      documents,
      railStore,
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
    });
    const second = await publish.requestPublish({
      specId,
      actorUserId: owner,
      actionId: randomUUID(),
      acknowledgeOpenQuestions: false,
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
      const first = scannerDeps(store);

      // Drive the machine four times. Every step after the first is a replay.
      await runSpecPublishTick(first.deps);
      await runSpecPublishTick(first.deps);
      const second = scannerDeps(store);
      await runSpecPublishTick(second.deps);
      await runSpecPublishTick(second.deps);

      const record = await store.readPublish(specId);
      expect(record?.state).toBe("complete");
      expect(record?.artifactVersion).toBe(1);

      const spec = await pool!.query<{
        phase: string;
        published_checkpoint_id: string | null;
        published_by: string | null;
        published_at: Date | null;
      }>(
        `SELECT phase, published_checkpoint_id, published_by, published_at
           FROM spec WHERE id = $1`,
        [specId],
      );
      expect(spec.rows[0]!.phase).toBe("published");
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

      // The hand-off always carries the same prompt id. The count is not
      // asserted: a driver that dies between the hand-off and the complete
      // mark repeats the call, and SendPrompt's prompt id is what makes that
      // repeat harmless (ADR 0067).
      const starts = [...first.ticketize.starts, ...second.ticketize.starts];
      expect(starts.length).toBeGreaterThanOrEqual(1);
      expect([...new Set(starts)]).toEqual([ticketizePromptId(specId)]);
    },
  );

  test.skipIf(!reachable)(
    "an unsettled section does not block the pin or run a gap check",
    async () => {
      const store = new PostgresSpecPublishStore(pool!);
      const publish = service(store);
      await publish.requestPublish({
        specId: raceSpecId,
        actorUserId: owner,
        actionId: randomUUID(),
        acknowledgeOpenQuestions: false,
      });
      await pool!.query(
        `UPDATE spec_section_state SET state = 'proposed'
          WHERE spec_id = $1 AND section_id = 'sec-data'`,
        [raceSpecId],
      );

      const result = await runSpecPublishTick(scannerDeps(store).deps);

      expect(result.blocked).toBe(0);
      expect(result.pinned).toBe(1);
      const record = await store.readPublish(raceSpecId);
      expect(record?.state).toBe("complete");
      const sideEffects = await pool!.query<{ checkpoints: number; gap_checks: number }>(
        `SELECT
           (SELECT count(*)::int FROM spec_checkpoint WHERE spec_id = $1) AS checkpoints,
           (SELECT count(*)::int FROM spec_gap_check_run WHERE spec_id = $1) AS gap_checks`,
        [raceSpecId],
      );
      expect(sideEffects.rows[0]).toEqual({ checkpoints: 1, gap_checks: 0 });
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
      });
    } catch (error) {
      if (!(error instanceof SpecPublishError)) throw error;
      refusal = error;
    }

    expect(refusal?.code).toBe("not_owner");
    expect(refusal?.status?.canPublish).toBe(false);
    // The member still reads the confirmation, because a spec is org-visible.
    expect(refusal?.status?.openQuestions).toEqual([]);
    const rows = await pool!.query<{ count: number }>(
      "SELECT count(*)::int AS count FROM spec_publish WHERE spec_id = $1",
      [otherSpecId],
    );
    expect(rows.rows[0]!.count).toBe(0);
    const spec = await pool!.query<{ phase: string }>(
      "SELECT phase FROM spec WHERE id = $1",
      [otherSpecId],
    );
    expect(spec.rows[0]!.phase).toBe("drafting");
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

    // The pin transaction commits once, whichever driver calls it again.
    const pinInput = {
      specId: otherSpecId,
      publishedBy: owner,
      at: now,
      checkpoint: {
        id: record.checkpointId,
        state: new Uint8Array([1]),
        stateVector: new Uint8Array([2]),
        renderedMarkdown: "# pinned\n",
        docSeq: 4n,
        label: "Published version",
        reason: "publish",
      },
      semanticDocSeq: 4n,
      acknowledgedQuestionIds: [] as string[],
    };
    expect(await store.pin(pinInput)).toEqual({ kind: "pinned" });
    expect(await store.pin(pinInput)).toEqual({ kind: "not_requested" });
    const checkpoints = await pool!.query<{ count: number }>(
      "SELECT count(*)::int AS count FROM spec_checkpoint WHERE spec_id = $1",
      [otherSpecId],
    );
    expect(checkpoints.rows[0]!.count).toBe(1);
    const published = await pool!.query<{ phase: string; published_checkpoint_id: string }>(
      "SELECT phase, published_checkpoint_id FROM spec WHERE id = $1",
      [otherSpecId],
    );
    expect(published.rows[0]!.phase).toBe("published");
    expect(published.rows[0]!.published_checkpoint_id).toBe(record.checkpointId);
  });
});
