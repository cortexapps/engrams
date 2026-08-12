/**
 * The publish gate and the publish record (ADR 0114 D10 and D12, R30, R34-R38).
 *
 * Publishing has one browser-facing step and three side-effectful ones. This
 * module owns the browser-facing step: it evaluates the gate, refuses a
 * publish that the gate blocks, and records the intent in one row. Every later
 * step belongs to the scanner in `publish-scanner.ts`, because the artifact leg
 * needs a live sandbox and the ticketize leg needs the session — neither may
 * ride on the lifetime of an HTTP request (ADR 0034).
 *
 * The row carries the checkpoint id and the artifact id, both minted with the
 * request. That is what makes the publish exactly-once however many times the
 * scanner drives it: the pin inserts one checkpoint at a known id, and the
 * artifact leg creates one artifact at a known id.
 *
 * v1 is owner-only (R37) and one-way (R38). There is no unpublish and no
 * revise: rework is a new spec, and the published spec stays readable.
 */

import {
  evaluatePublishGate,
  type PublishGate,
  type PublishGateQuestion,
  type PublishGateSection,
} from "@engrams/spec-document";
import { randomUUID } from "node:crypto";
import type { Node as ProseMirrorNode } from "prosemirror-model";
import type { Pool } from "pg";

import type { SpecPublishState } from "../db/schema.ts";
import type { SpecRailMetadata, SpecRailStore } from "../routes/spec-rail.ts";
import { proseMirrorDocument, type SpecDocumentService } from "./doc-service.ts";
import { GapCheckError, readSections } from "./gap-check.ts";
import type { GapCheckService } from "./gap-check.ts";

/** What the publish path needs to know about the spec row itself. */
export interface SpecPublishTarget {
  specId: string;
  title: string;
  lifecycle: "draft" | "published";
  ownerUserId: string | null;
  sessionId: string | null;
  publishedCheckpointId: string | null;
  publishedAt: Date | null;
  /** `off` means this template does not run the gap check, so it cannot gate. */
  gapCheckStage: "on" | "suggested" | "off";
}

/** The durable publish record — one row per spec. */
export interface SpecPublishRecord {
  specId: string;
  sessionId: string;
  checkpointId: string;
  artifactId: string;
  artifactVersion: number | null;
  state: SpecPublishState;
  requestedBy: string | null;
  requestedAt: Date;
  acknowledgedQuestionCount: number;
  acknowledgedQuestionIds: string[];
  gapCheckRunId: string | null;
  attempts: number;
  nextAttemptAt: Date;
  lastError: string | null;
  pinnedAt: Date | null;
  completedAt: Date | null;
}

/** A claimed row, with the spec fields the scanner's steps need. */
export interface SpecPublishWork extends SpecPublishRecord {
  specTitle: string;
  ownerUserId: string | null;
}

export interface SpecPublishStore {
  readTarget(specId: string): Promise<SpecPublishTarget | null>;
  readPublish(specId: string): Promise<SpecPublishRecord | null>;
  listOpenQuestions(specId: string): Promise<Array<{ id: string; sectionId: string; text: string }>>;
  /** Records the intent. A second request for the same spec changes nothing. */
  insertRequest(record: SpecPublishRecord): Promise<SpecPublishRecord>;
  /**
   * Claim the rows that are due, pushing each claim forward by the retry
   * delay. The push is both the backoff and the exclusion: a second pod's
   * statement re-reads `next_attempt_at` and finds nothing to claim.
   *
   * `specId` restricts the claim to one spec, so a push wake never consumes
   * another spec's turn.
   */
  claimDue(input: {
    now: Date;
    retryAt: Date;
    limit: number;
    specId?: string;
  }): Promise<SpecPublishWork[]>;
  /**
   * Pin the checkpoint and flip the spec to published, in one transaction.
   * False when another driver already pinned it.
   */
  markPinned(input: {
    specId: string;
    checkpointId: string;
    publishedBy: string | null;
    at: Date;
  }): Promise<boolean>;
  markArtifactPublished(input: { specId: string; version: number }): Promise<boolean>;
  markComplete(input: { specId: string; at: Date }): Promise<boolean>;
  recordFailure(input: { specId: string; error: string; retryAt: Date }): Promise<void>;
}

/** Everything the browser needs to render the button and the dialog. */
export interface SpecPublishStatus {
  lifecycle: "draft" | "published";
  gate: PublishGate;
  gapCheck: { stale: boolean; runId: string | null; ranAt: Date | null; gates: boolean };
  publish: SpecPublishRecord | null;
  /** True when the caller may publish this spec (R37). */
  canPublish: boolean;
  publishedAt: Date | null;
}

export interface RequestPublishInput {
  specId: string;
  actorUserId: string;
  /** A stable id, so a replayed request runs one gap check, not two. */
  actionId: string;
  /** The person acknowledged the open questions in full (R35). */
  acknowledgeOpenQuestions: boolean;
  /** The person accepted running the gap check as part of the publish (R30). */
  runGapCheck: boolean;
}

export interface RequestPublishResult {
  publish: SpecPublishRecord;
  status: SpecPublishStatus;
  /** False when the record already existed, so this call changed nothing. */
  created: boolean;
}

export class SpecPublishError extends Error {
  constructor(
    readonly code:
      | "spec_not_found"
      | "not_owner"
      | "no_session"
      | "already_published"
      | "blocked"
      | "acknowledgment_required"
      | "gap_check_stale"
      | "gap_check_failed",
    message: string,
    /** The gate at refusal time, so the dialog can list what to fix. */
    readonly status?: SpecPublishStatus,
  ) {
    super(message);
    this.name = "SpecPublishError";
  }
}

export interface SpecPublishServiceOptions {
  store: SpecPublishStore;
  railStore: SpecRailStore;
  documents: Pick<SpecDocumentService, "syncFromLog">;
  gapCheck: Pick<GapCheckService, "status" | "run">;
  now: () => Date;
  newId?: () => string;
}

export class SpecPublishService {
  private readonly newId: () => string;

  constructor(private readonly options: SpecPublishServiceOptions) {
    this.newId = options.newId ?? randomUUID;
  }

  /** The gate as it stands, for one caller. Every org member may read it. */
  async status(specId: string, actorUserId: string): Promise<SpecPublishStatus> {
    const target = await this.options.store.readTarget(specId);
    if (!target) {
      throw new SpecPublishError("spec_not_found", `Spec ${specId} does not exist.`);
    }
    return this.statusFor(target, actorUserId);
  }

  /**
   * Record the publish intent, after the gate passes.
   *
   * The gate is evaluated here and nowhere else. A blocked publish never
   * reaches the state machine, so an unsettled required section cannot become
   * a pinned version.
   */
  async requestPublish(input: RequestPublishInput): Promise<RequestPublishResult> {
    const target = await this.options.store.readTarget(input.specId);
    if (!target) {
      throw new SpecPublishError("spec_not_found", `Spec ${input.specId} does not exist.`);
    }
    if (target.ownerUserId !== input.actorUserId) {
      throw new SpecPublishError(
        "not_owner",
        "Only the spec owner publishes this spec.",
        await this.statusFor(target, input.actorUserId),
      );
    }

    // A publish already recorded is the whole answer: the state machine owns
    // the rest, and a second request must not pin a second version (R38).
    const existing = await this.options.store.readPublish(input.specId);
    if (existing) {
      return {
        publish: existing,
        status: await this.statusFor(target, input.actorUserId),
        created: false,
      };
    }
    if (target.lifecycle === "published") {
      throw new SpecPublishError(
        "already_published",
        "This spec is already published. Rework means a new spec.",
        await this.statusFor(target, input.actorUserId),
      );
    }
    if (!target.sessionId) {
      throw new SpecPublishError(
        "no_session",
        "This spec has no drafting session, so it cannot publish an artifact.",
        await this.statusFor(target, input.actorUserId),
      );
    }

    let status = await this.statusFor(target, input.actorUserId);
    if (!status.gate.ready) {
      throw new SpecPublishError(
        "blocked",
        `${status.gate.blockers.length} required sections are not settled.`,
        status,
      );
    }
    // R30: the gap check runs as part of the publish when it is stale for this
    // drafting round. The dialog stays open through the run.
    if (status.gate.gapCheckRunRequired) {
      if (!input.runGapCheck) {
        throw new SpecPublishError(
          "gap_check_stale",
          "The gap check has not run for this revision of the spec.",
          status,
        );
      }
      try {
        await this.options.gapCheck.run({
          specId: input.specId,
          sessionId: target.sessionId,
          requestFingerprint: `publish-gate:${input.actionId}`,
          actorUserId: input.actorUserId,
        });
      } catch (error) {
        // The pass could not read the document — most often a requirement
        // ledger it cannot trace. Say so, rather than publish without the
        // check or fail with a bare 500.
        if (!(error instanceof GapCheckError)) throw error;
        throw new SpecPublishError("gap_check_failed", error.message, status);
      }
      status = await this.statusFor(target, input.actorUserId);
      // The pass can find nothing to change, but a person may have settled a
      // section between the two reads, so the gate is re-checked, not assumed.
      if (!status.gate.ready) {
        throw new SpecPublishError(
          "blocked",
          `${status.gate.blockers.length} required sections are not settled.`,
          status,
        );
      }
    }
    if (status.gate.acknowledgmentRequired && !input.acknowledgeOpenQuestions) {
      throw new SpecPublishError(
        "acknowledgment_required",
        `${status.gate.openQuestions.length} open questions need an acknowledgment.`,
        status,
      );
    }

    const now = this.options.now();
    const record: SpecPublishRecord = {
      specId: input.specId,
      sessionId: target.sessionId,
      checkpointId: this.newId(),
      artifactId: this.newId(),
      artifactVersion: null,
      state: "requested",
      requestedBy: input.actorUserId,
      requestedAt: now,
      acknowledgedQuestionCount: status.gate.openQuestions.length,
      acknowledgedQuestionIds: status.gate.openQuestions.map((question) => question.id),
      gapCheckRunId: status.gapCheck.runId,
      attempts: 0,
      nextAttemptAt: now,
      lastError: null,
      pinnedAt: null,
      completedAt: null,
    };
    const stored = await this.options.store.insertRequest(record);
    return {
      publish: stored,
      status: { ...status, publish: stored },
      created: stored.checkpointId === record.checkpointId,
    };
  }

  private async statusFor(
    target: SpecPublishTarget,
    actorUserId: string,
  ): Promise<SpecPublishStatus> {
    const metadata = await this.options.railStore.readMetadata(target.specId);
    if (!metadata) {
      throw new SpecPublishError("spec_not_found", `Spec ${target.specId} does not exist.`);
    }
    const loaded = await this.options.documents.syncFromLog(target.specId);
    const sections = publishGateSections(proseMirrorDocument(loaded.doc), metadata);
    const openQuestions = await this.openQuestions(target.specId, sections);
    const gapCheck = await this.options.gapCheck.status(target.specId);
    // A template with the gap check off cannot make a stale pass a gate (R30).
    const gates = target.gapCheckStage !== "off";
    const gate = evaluatePublishGate({
      sections,
      openQuestions,
      gapCheckStale: gates && gapCheck.stale,
    });
    const publish = await this.options.store.readPublish(target.specId);
    return {
      lifecycle: target.lifecycle,
      gate,
      gapCheck: {
        stale: gapCheck.stale,
        runId: gapCheck.run?.id ?? null,
        ranAt: gapCheck.run?.createdAt ?? null,
        gates,
      },
      publish,
      canPublish:
        target.ownerUserId === actorUserId && target.lifecycle === "draft" && publish === null,
      publishedAt: target.publishedAt,
    };
  }

  private async openQuestions(
    specId: string,
    sections: readonly PublishGateSection[],
  ): Promise<PublishGateQuestion[]> {
    const rows = await this.options.store.listOpenQuestions(specId);
    const titles = new Map(sections.map((section) => [section.id, section.title]));
    return rows.map((row) => ({
      id: row.id,
      sectionId: row.sectionId,
      sectionTitle: titles.get(row.sectionId) ?? row.sectionId,
      text: row.text,
    }));
  }
}

/**
 * Read the gate's view of the document. The section walk is the gap check's
 * (`readSections`), so both surfaces see one section list; the gate adds the
 * template's `required` flag and the recorded `n/a` reason.
 */
export function publishGateSections(
  document: ProseMirrorNode,
  metadata: SpecRailMetadata,
): PublishGateSection[] {
  const rules = new Map(metadata.sections.map((section) => [section.key, section]));
  return readSections(document, metadata).map((section) => {
    const rule = rules.get(section.key);
    if (!rule) throw new Error(`Spec section ${section.id} has no template rule.`);
    return {
      id: section.id,
      title: section.title,
      layerKey: section.layerKey,
      required: rule.required,
      state: section.state,
      naReason: metadata.states.get(section.id)?.naReason ?? null,
    };
  });
}

interface SpecPublishTargetRow {
  id: string;
  title: string;
  lifecycle: string;
  owner_user_id: string | null;
  session_id: string | null;
  published_checkpoint_id: string | null;
  published_at: Date | null;
  gap_check_stage: string | null;
}

interface SpecPublishRow {
  spec_id: string;
  session_id: string;
  checkpoint_id: string;
  artifact_id: string;
  artifact_version: number | null;
  state: SpecPublishState;
  requested_by: string | null;
  requested_at: Date;
  acknowledged_question_count: number;
  acknowledged_question_ids: string[];
  gap_check_run_id: string | null;
  attempts: number;
  next_attempt_at: Date;
  last_error: string | null;
  pinned_at: Date | null;
  completed_at: Date | null;
}

interface SpecPublishWorkRow extends SpecPublishRow {
  spec_title: string;
  owner_user_id: string | null;
}

const PUBLISH_COLUMNS = `spec_id, session_id, checkpoint_id, artifact_id, artifact_version,
          state, requested_by, requested_at, acknowledged_question_count,
          acknowledged_question_ids, gap_check_run_id, attempts, next_attempt_at,
          last_error, pinned_at, completed_at`;

export class PostgresSpecPublishStore implements SpecPublishStore {
  constructor(private readonly pool: Pool) {}

  async readTarget(specId: string): Promise<SpecPublishTarget | null> {
    const result = await this.pool.query<SpecPublishTargetRow>(
      `SELECT spec.id, spec.title, spec.lifecycle, spec.owner_user_id, spec.session_id,
              spec.published_checkpoint_id, spec.published_at,
              template.stage_flags->>'gapCheck' AS gap_check_stage
         FROM spec
         JOIN spec_template AS template ON template.id = spec.template_id
        WHERE spec.id = $1`,
      [specId],
    );
    const row = result.rows[0];
    if (!row) return null;
    if (row.lifecycle !== "draft" && row.lifecycle !== "published") {
      throw new Error(`Spec ${specId} has an invalid lifecycle: ${row.lifecycle}`);
    }
    return {
      specId: row.id,
      title: row.title,
      lifecycle: row.lifecycle,
      ownerUserId: row.owner_user_id,
      sessionId: row.session_id,
      publishedCheckpointId: row.published_checkpoint_id,
      publishedAt: row.published_at,
      gapCheckStage: stageMode(row.gap_check_stage),
    };
  }

  async readPublish(specId: string): Promise<SpecPublishRecord | null> {
    const result = await this.pool.query<SpecPublishRow>(
      `SELECT ${PUBLISH_COLUMNS} FROM spec_publish WHERE spec_id = $1`,
      [specId],
    );
    const row = result.rows[0];
    return row ? publishRecord(row) : null;
  }

  async listOpenQuestions(
    specId: string,
  ): Promise<Array<{ id: string; sectionId: string; text: string }>> {
    const result = await this.pool.query<{ id: string; section_id: string; text: string }>(
      `SELECT id, section_id, text
         FROM spec_open_question
        WHERE spec_id = $1 AND state = 'open'
        ORDER BY created_at, id`,
      [specId],
    );
    return result.rows.map((row) => ({ id: row.id, sectionId: row.section_id, text: row.text }));
  }

  async insertRequest(record: SpecPublishRecord): Promise<SpecPublishRecord> {
    await this.pool.query(
      `INSERT INTO spec_publish
         (spec_id, session_id, checkpoint_id, artifact_id, state, requested_by, requested_at,
          acknowledged_question_count, acknowledged_question_ids, gap_check_run_id,
          attempts, next_attempt_at)
       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 0, $11)
       ON CONFLICT (spec_id) DO NOTHING`,
      [
        record.specId,
        record.sessionId,
        record.checkpointId,
        record.artifactId,
        record.state,
        record.requestedBy,
        record.requestedAt,
        record.acknowledgedQuestionCount,
        JSON.stringify(record.acknowledgedQuestionIds),
        record.gapCheckRunId,
        record.nextAttemptAt,
      ],
    );
    const stored = await this.readPublish(record.specId);
    if (!stored) throw new Error(`The publish request for spec ${record.specId} was not stored.`);
    return stored;
  }

  async claimDue(input: {
    now: Date;
    retryAt: Date;
    limit: number;
    specId?: string;
  }): Promise<SpecPublishWork[]> {
    const result = await this.pool.query<SpecPublishWorkRow>(
      `WITH claimed AS (
         UPDATE spec_publish
            SET next_attempt_at = $2, attempts = attempts + 1
          WHERE spec_id IN (
                  SELECT spec_id
                    FROM spec_publish
                   WHERE state <> 'complete'
                     AND next_attempt_at <= $1
                     AND ($4::uuid IS NULL OR spec_id = $4::uuid)
                   ORDER BY next_attempt_at
                   LIMIT $3
                )
            AND state <> 'complete'
            AND next_attempt_at <= $1
        RETURNING ${PUBLISH_COLUMNS}
       )
       SELECT claimed.*, spec.title AS spec_title, spec.owner_user_id
         FROM claimed
         JOIN spec ON spec.id = claimed.spec_id`,
      [input.now, input.retryAt, input.limit, input.specId ?? null],
    );
    return result.rows.map((row) => ({
      ...publishRecord(row),
      specTitle: row.spec_title,
      ownerUserId: row.owner_user_id,
    }));
  }

  async markPinned(input: {
    specId: string;
    checkpointId: string;
    publishedBy: string | null;
    at: Date;
  }): Promise<boolean> {
    const client = await this.pool.connect();
    try {
      await client.query("BEGIN");
      const advanced = await client.query(
        `UPDATE spec_publish
            SET state = 'pinned', pinned_at = $2, last_error = NULL
          WHERE spec_id = $1 AND state = 'requested'`,
        [input.specId, input.at],
      );
      if (advanced.rowCount === 0) {
        await client.query("ROLLBACK");
        return false;
      }
      // The flip and the pin commit together: a published spec always has its
      // pinned checkpoint, and the document store refuses edits from here on.
      const flipped = await client.query(
        `UPDATE spec
            SET lifecycle = 'published', published_checkpoint_id = $2,
                published_by = $3, published_at = $4, updated_at = $4
          WHERE id = $1 AND lifecycle = 'draft'`,
        [input.specId, input.checkpointId, input.publishedBy, input.at],
      );
      if (flipped.rowCount === 0) {
        await client.query("ROLLBACK");
        return false;
      }
      await client.query("COMMIT");
      return true;
    } catch (error) {
      await rollbackQuietly(client);
      throw error;
    } finally {
      client.release();
    }
  }

  async markArtifactPublished(input: { specId: string; version: number }): Promise<boolean> {
    const result = await this.pool.query(
      `UPDATE spec_publish
          SET state = 'artifact_published', artifact_version = $2, last_error = NULL
        WHERE spec_id = $1 AND state = 'pinned'`,
      [input.specId, input.version],
    );
    return (result.rowCount ?? 0) > 0;
  }

  async markComplete(input: { specId: string; at: Date }): Promise<boolean> {
    const result = await this.pool.query(
      `UPDATE spec_publish
          SET state = 'complete', completed_at = $2, last_error = NULL
        WHERE spec_id = $1 AND state = 'artifact_published'`,
      [input.specId, input.at],
    );
    return (result.rowCount ?? 0) > 0;
  }

  async recordFailure(input: { specId: string; error: string; retryAt: Date }): Promise<void> {
    await this.pool.query(
      `UPDATE spec_publish SET last_error = $2, next_attempt_at = $3 WHERE spec_id = $1`,
      [input.specId, input.error.slice(0, 2000), input.retryAt],
    );
  }
}

function publishRecord(row: SpecPublishRow): SpecPublishRecord {
  return {
    specId: row.spec_id,
    sessionId: row.session_id,
    checkpointId: row.checkpoint_id,
    artifactId: row.artifact_id,
    artifactVersion: row.artifact_version,
    state: row.state,
    requestedBy: row.requested_by,
    requestedAt: row.requested_at,
    acknowledgedQuestionCount: row.acknowledged_question_count,
    acknowledgedQuestionIds: row.acknowledged_question_ids,
    gapCheckRunId: row.gap_check_run_id,
    attempts: row.attempts,
    nextAttemptAt: row.next_attempt_at,
    lastError: row.last_error,
    pinnedAt: row.pinned_at,
    completedAt: row.completed_at,
  };
}

function stageMode(value: string | null): "on" | "suggested" | "off" {
  return value === "off" || value === "suggested" ? value : "on";
}

async function rollbackQuietly(client: { query: (sql: string) => Promise<unknown> }): Promise<void> {
  try {
    await client.query("ROLLBACK");
  } catch {
    // The connection is already broken; the caller's error is the real one.
  }
}
