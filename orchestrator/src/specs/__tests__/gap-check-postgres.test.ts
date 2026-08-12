import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { Pool } from "pg";

import { PostgresGapCheckStore, type GapCheckRun } from "../gap-check.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const pool: Pool | null = DB_URL ? new Pool({ connectionString: DB_URL, max: 4 }) : null;
let reachable = false;

if (pool) {
  reachable = await pool
    .query("SELECT 1")
    .then(() => true)
    .catch(() => false);
}

describe("PostgresGapCheckStore with live Postgres", () => {
  const templateId = randomUUID();
  const specId = randomUUID();
  const actorUserId = `gap-check-actor-${randomUUID()}`;

  beforeAll(async () => {
    if (!reachable || !pool) return;
    await pool.query(
      `INSERT INTO spec_template (id, name, layers, sections, stage_flags)
       VALUES ($1, 'Gap check test template', '[]', '[]', '{}')`,
      [templateId],
    );
    await pool.query(
      `INSERT INTO spec (id, org_id, template_id, title, lifecycle)
       VALUES ($1, 'test-org', $2, 'Gap check test spec', 'draft')`,
      [specId, templateId],
    );
    await pool.query(
      `INSERT INTO "user" (id, name, email, email_verified, created_at, updated_at)
       VALUES ($1, 'Ari', $1 || '@example.test', false, now(), now())`,
      [actorUserId],
    );
  });

  afterAll(async () => {
    if (!reachable || !pool) return;
    await pool.query("DELETE FROM spec WHERE id = $1", [specId]);
    await pool.query("DELETE FROM spec_template WHERE id = $1", [templateId]);
    await pool.query('DELETE FROM "user" WHERE id = $1', [actorUserId]);
    await pool.end();
  });

  function run(overrides: Partial<GapCheckRun> = {}): GapCheckRun {
    return {
      id: randomUUID(),
      specId,
      sessionId: null,
      semanticDocSeq: 12n,
      stoppedAtLayerKey: "contract",
      suppressedCount: 3,
      matrix: {
        layers: [{ key: "contract", title: "Contract" }],
        rows: [
          {
            requirementId: "R1",
            label: "an org caps its sandboxes",
            verdict: "gap",
            cells: [
              {
                layerKey: "contract",
                covered: false,
                citations: [],
                note: "no Contract content cites R1",
              },
            ],
          },
        ],
      },
      startedBy: actorUserId,
      createdAt: new Date("2026-08-12T14:31:00.000Z"),
      findings: [
        {
          id: "requirement_gap:R1:contract",
          kind: "requirement_gap",
          severity: "gap",
          layerKey: "contract",
          sectionId: "sec-behavior",
          sectionTitle: "Behavior",
          requirementId: "R1",
          summary: "R1 has no Contract coverage",
          detail: "R1 is not cited anywhere in Contract.",
          proposedDiff: { sectionId: "sec-behavior", before: "old", after: "new" },
          disposition: "pending",
          openQuestionId: null,
          disposedBy: null,
          disposedAt: null,
        },
        {
          id: "red_team:sec-behavior:0",
          kind: "red_team",
          severity: "fatal",
          layerKey: "contract",
          sectionId: "sec-behavior",
          sectionTitle: "Behavior",
          requirementId: null,
          summary: "the reset promise contradicts the cache TTL",
          detail: "Resolve this before the layers below it.",
          proposedDiff: null,
          disposition: "pending",
          openQuestionId: null,
          disposedBy: null,
          disposedAt: null,
        },
      ],
      ...overrides,
    };
  }

  test.skipIf(!reachable)("a run round-trips with its findings in reported order", async () => {
    const store = new PostgresGapCheckStore(pool!);
    const original = run();

    await store.insertRun(original, "fingerprint-round-trip");
    const stored = await store.findRunByFingerprint(specId, "fingerprint-round-trip");

    expect(stored).not.toBeNull();
    expect(stored!.id).toBe(original.id);
    expect(stored!.semanticDocSeq).toBe(12n);
    expect(stored!.stoppedAtLayerKey).toBe("contract");
    expect(stored!.suppressedCount).toBe(3);
    expect(stored!.startedBy).toBe(actorUserId);
    expect(stored!.matrix.rows[0]!.cells[0]!.note).toBe("no Contract content cites R1");
    expect(stored!.findings.map((finding) => finding.id)).toEqual([
      "requirement_gap:R1:contract",
      "red_team:sec-behavior:0",
    ]);
    expect(stored!.findings[0]!.proposedDiff).toEqual({
      sectionId: "sec-behavior",
      before: "old",
      after: "new",
    });
    expect(stored!.findings[1]!.proposedDiff).toBeNull();
    expect(stored!.findings[1]!.requirementId).toBeNull();
  });

  test.skipIf(!reachable)("a replayed fingerprint records one run, not two", async () => {
    const store = new PostgresGapCheckStore(pool!);
    const first = run();
    const second = run();

    await store.insertRun(first, "fingerprint-replay");
    await store.insertRun(second, "fingerprint-replay");

    const stored = await store.findRunByFingerprint(specId, "fingerprint-replay");
    expect(stored!.id).toBe(first.id);
    // The losing run left no orphan findings behind.
    const orphans = await pool!.query(
      "SELECT count(*)::int AS count FROM spec_gap_check_finding WHERE run_id = $1",
      [second.id],
    );
    expect(orphans.rows[0].count).toBe(0);
  });

  test.skipIf(!reachable)("a finding is disposed of exactly once", async () => {
    const store = new PostgresGapCheckStore(pool!);
    const original = run();
    await store.insertRun(original, "fingerprint-dispose");
    const disposedAt = new Date("2026-08-12T14:40:00.000Z");
    const questionId = randomUUID();

    const first = await store.markDisposition({
      runId: original.id,
      findingId: "requirement_gap:R1:contract",
      disposition: "question_opened",
      openQuestionId: questionId,
      disposedBy: actorUserId,
      disposedAt,
    });
    const second = await store.markDisposition({
      runId: original.id,
      findingId: "requirement_gap:R1:contract",
      disposition: "dismissed",
      openQuestionId: null,
      disposedBy: actorUserId,
      disposedAt,
    });

    expect(first).toBe(true);
    expect(second).toBe(false);
    const stored = await store.readRun(original.id);
    const finding = stored!.findings.find(
      (candidate) => candidate.id === "requirement_gap:R1:contract",
    )!;
    expect(finding.disposition).toBe("question_opened");
    expect(finding.openQuestionId).toBe(questionId);
    expect(finding.disposedBy).toBe(actorUserId);
    expect(finding.disposedAt).toEqual(disposedAt);
  });

  test.skipIf(!reachable)("latestRun reads the newest pass for the spec", async () => {
    const store = new PostgresGapCheckStore(pool!);
    const older = run({ createdAt: new Date("2026-08-12T10:00:00.000Z") });
    const newer = run({
      createdAt: new Date("2026-08-12T18:00:00.000Z"),
      semanticDocSeq: 19n,
    });

    await store.insertRun(older, "fingerprint-older");
    await store.insertRun(newer, "fingerprint-newer");

    const latest = await store.latestRun(specId);
    expect(latest!.id).toBe(newer.id);
    expect(latest!.semanticDocSeq).toBe(19n);
  });

  test.skipIf(!reachable)("deleting the spec removes its runs and findings", async () => {
    const throwawayTemplateId = randomUUID();
    const throwawaySpecId = randomUUID();
    await pool!.query(
      `INSERT INTO spec_template (id, name, layers, sections, stage_flags)
       VALUES ($1, 'Cascade template', '[]', '[]', '{}')`,
      [throwawayTemplateId],
    );
    await pool!.query(
      `INSERT INTO spec (id, org_id, template_id, title, lifecycle)
       VALUES ($1, 'test-org', $2, 'Cascade spec', 'draft')`,
      [throwawaySpecId, throwawayTemplateId],
    );
    const store = new PostgresGapCheckStore(pool!);
    const original = run({ specId: throwawaySpecId });
    await store.insertRun(original, "fingerprint-cascade");

    await pool!.query("DELETE FROM spec WHERE id = $1", [throwawaySpecId]);

    const findings = await pool!.query(
      "SELECT count(*)::int AS count FROM spec_gap_check_finding WHERE run_id = $1",
      [original.id],
    );
    expect(findings.rows[0].count).toBe(0);
    expect(await store.readRun(original.id)).toBeNull();
    await pool!.query("DELETE FROM spec_template WHERE id = $1", [throwawayTemplateId]);
  });
});
