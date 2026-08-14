/**
 * The publish confirmation and durable publish record (ADR 0114 D10 and D12).
 *
 * Publishing has one browser-facing step and three side-effectful ones. This
 * module owns the browser-facing confirmation and records the intent in one
 * row. Every later step belongs to the scanner in `publish-scanner.ts`, because
 * the artifact leg needs a live sandbox and the ticketize leg needs the session.
 * Neither may ride on the lifetime of an HTTP request (ADR 0034).
 *
 * The row carries the checkpoint id and the artifact id, both minted with the
 * request. That is what makes the publish exactly-once however many times the
 * scanner drives it: the pin inserts one checkpoint at a known id, and the
 * artifact leg creates one artifact at a known id.
 *
 * Publishing is owner-only and one-way. Open questions need an explicit
 * acknowledgment, but document completeness and gap checks do not gate it.
 */

import { randomUUID } from "node:crypto";
import type { Pool } from "pg";

import type { SpecPublishState } from "../db/schema.ts";
import type { SpecRailMetadata, SpecRailStore } from "../routes/spec-rail.ts";
import { proseMirrorDocument, type SpecDocumentService } from "./doc-service.ts";
import { readSections } from "./gap-check.ts";

/** What the publish path needs to know about the spec row itself. */
export interface SpecPublishTarget {
  specId: string;
  title: string;
  phase: "ideation" | "drafting" | "published";
  ownerUserId: string | null;
  sessionId: string | null;
  publishedCheckpointId: string | null;
  publishedAt: Date | null;
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
   * another spec's turn. A `blocked` row waits for a new acknowledgment.
   */
  claimDue(input: {
    now: Date;
    retryAt: Date;
    limit: number;
    specId?: string;
  }): Promise<SpecPublishWork[]>;
  /**
   * Insert the pinned checkpoint, flip the spec to published and advance the
   * publish row in one transaction. The transaction confirms that no new open
   * question appeared after the owner acknowledged the list.
   */
  pin(input: PinPublishInput): Promise<PinOutcome>;
  markArtifactPublished(input: { specId: string; version: number }): Promise<boolean>;
  markComplete(input: { specId: string; at: Date }): Promise<boolean>;
  /** Terminal refusal: the open-question acknowledgment is no longer current. */
  markBlocked(input: { specId: string; reason: string; at: Date }): Promise<boolean>;
  /** Re-arm a blocked publish after the owner asks again. */
  resetBlocked(input: {
    specId: string;
    requestedBy: string | null;
    requestedAt: Date;
    acknowledgedQuestionIds: readonly string[];
    gapCheckRunId: string | null;
  }): Promise<boolean>;
  recordFailure(input: { specId: string; error: string; retryAt: Date }): Promise<void>;
}

export interface PinPublishInput {
  specId: string;
  publishedBy: string | null;
  at: Date;
  /** The compacted document this pin commits, rendered and encoded. */
  checkpoint: {
    id: string;
    state: Uint8Array;
    stateVector: Uint8Array;
    renderedMarkdown: string;
    docSeq: bigint;
    label: string;
    reason: string;
  };
  /** The revision the confirmation was made against. */
  semanticDocSeq: bigint;
  /** The questions the owner acknowledged carrying into the tickets. */
  acknowledgedQuestionIds: readonly string[];
}

export type PinOutcome =
  /** The pin committed: checkpoint, phase flip and publish row together. */
  | { kind: "pinned" }
  /** The document moved past the compaction. Recompact and try again. */
  | { kind: "stale_document"; currentSemanticDocSeq: bigint }
  /** A new open question needs a fresh acknowledgment. */
  | { kind: "confirmation_failed"; reason: string }
  /** Another driver already advanced this row. */
  | { kind: "not_requested" };

export interface SpecPublishOpenQuestion {
  id: string;
  sectionId: string;
  sectionTitle: string;
  text: string;
}

/** Everything the browser needs to render the button and confirmation. */
export interface SpecPublishStatus {
  phase: "ideation" | "drafting" | "published";
  openQuestions: SpecPublishOpenQuestion[];
  publish: SpecPublishRecord | null;
  /** True when the caller may publish this spec. */
  canPublish: boolean;
}

export interface RequestPublishInput {
  specId: string;
  actorUserId: string;
  /** A stable id for this confirmation. */
  actionId: string;
  /** The person acknowledged the open questions in full. */
  acknowledgeOpenQuestions: boolean;
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
      | "ideation"
      | "already_published"
      | "acknowledgment_required",
    message: string,
    /** The confirmation state at refusal time, so the dialog can list questions. */
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
  now: () => Date;
  newId?: () => string;
}

export class SpecPublishService {
  private readonly newId: () => string;

  constructor(private readonly options: SpecPublishServiceOptions) {
    this.newId = options.newId ?? randomUUID;
  }

  /** The confirmation state for one caller. Every org member may read it. */
  async status(specId: string, actorUserId: string): Promise<SpecPublishStatus> {
    const target = await this.options.store.readTarget(specId);
    if (!target) {
      throw new SpecPublishError("spec_not_found", `Spec ${specId} does not exist.`);
    }
    return this.statusFor(target, actorUserId);
  }

  /**
   * Record the publish intent after the owner confirms the open questions.
   * The scanner pins the document and completes the durable pipeline.
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
    // the rest, and a second request must not pin a second version. A blocked
    // one is the exception because it pinned nothing.
    const existing = await this.options.store.readPublish(input.specId);
    if (existing && existing.state !== "blocked") {
      return {
        publish: existing,
        status: await this.statusFor(target, input.actorUserId),
        created: false,
      };
    }
    if (target.phase === "published") {
      throw new SpecPublishError(
        "already_published",
        "This spec is already published. Rework means a new spec.",
        await this.statusFor(target, input.actorUserId),
      );
    }
    if (target.phase === "ideation") {
      throw new SpecPublishError(
        "ideation",
        "Start drafting before you publish this spec.",
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

    const status = await this.statusFor(target, input.actorUserId);
    if (status.openQuestions.length > 0 && !input.acknowledgeOpenQuestions) {
      throw new SpecPublishError(
        "acknowledgment_required",
        `${status.openQuestions.length} open questions need an acknowledgment.`,
        status,
      );
    }

    const now = this.options.now();
    const acknowledgedQuestionIds = status.openQuestions.map((question) => question.id);
    if (existing) {
      // A blocked publish keeps its checkpoint and artifact ids, because it
      // created neither. Only the acknowledgment and the attempt state are new.
      await this.options.store.resetBlocked({
        specId: input.specId,
        requestedBy: input.actorUserId,
        requestedAt: now,
        acknowledgedQuestionIds,
        gapCheckRunId: null,
      });
      const stored = await this.options.store.readPublish(input.specId);
      if (!stored) {
        throw new SpecPublishError("spec_not_found", `Spec ${input.specId} does not exist.`);
      }
      return { publish: stored, status: { ...status, publish: stored }, created: true };
    }

    const record: SpecPublishRecord = {
      specId: input.specId,
      sessionId: target.sessionId,
      checkpointId: this.newId(),
      artifactId: this.newId(),
      artifactVersion: null,
      state: "requested",
      requestedBy: input.actorUserId,
      requestedAt: now,
      acknowledgedQuestionCount: acknowledgedQuestionIds.length,
      acknowledgedQuestionIds,
      gapCheckRunId: null,
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
    const openQuestions = await this.openQuestions(target.specId);
    const publish = await this.options.store.readPublish(target.specId);
    return {
      phase: target.phase,
      openQuestions,
      publish,
      canPublish:
        target.ownerUserId === actorUserId &&
        target.phase === "drafting" &&
        (publish === null || publish.state === "blocked"),
    };
  }

  private async openQuestions(specId: string): Promise<SpecPublishOpenQuestion[]> {
    const rows = await this.options.store.listOpenQuestions(specId);
    if (rows.length === 0) return [];
    const metadata = await this.options.railStore.readMetadata(specId);
    if (!metadata) {
      throw new SpecPublishError("spec_not_found", `Spec ${specId} does not exist.`);
    }
    const loaded = await this.options.documents.syncFromLog(specId);
    const titles = new Map(
      readSections(proseMirrorDocument(loaded.doc), metadata).map((section) => [
        section.id,
        section.title,
      ]),
    );
    return rows.map((row) => ({
      id: row.id,
      sectionId: row.sectionId,
      sectionTitle: titles.get(row.sectionId) ?? row.sectionId,
      text: row.text,
    }));
  }
}

interface SpecPublishTargetRow {
  id: string;
  title: string;
  phase: string;
  owner_user_id: string | null;
  session_id: string | null;
  published_checkpoint_id: string | null;
  published_at: Date | null;
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
      `SELECT spec.id, spec.title, spec.phase, spec.owner_user_id, spec.session_id,
              spec.published_checkpoint_id, spec.published_at
         FROM spec
        WHERE spec.id = $1`,
      [specId],
    );
    const row = result.rows[0];
    if (!row) return null;
    if (row.phase !== "ideation" && row.phase !== "drafting" && row.phase !== "published") {
      throw new Error(`Spec ${specId} has an invalid phase: ${row.phase}`);
    }
    return {
      specId: row.id,
      title: row.title,
      phase: row.phase,
      ownerUserId: row.owner_user_id,
      sessionId: row.session_id,
      publishedCheckpointId: row.published_checkpoint_id,
      publishedAt: row.published_at,
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
                   WHERE state NOT IN ('complete', 'blocked')
                     AND next_attempt_at <= $1
                     AND ($4::uuid IS NULL OR spec_id = $4::uuid)
                   ORDER BY next_attempt_at
                   LIMIT $3
                )
            AND state NOT IN ('complete', 'blocked')
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

  async pin(input: PinPublishInput): Promise<PinOutcome> {
    const client = await this.pool.connect();
    try {
      await client.query("BEGIN");
      // Every document and open-question writer takes this row first, so
      // holding it makes the revision and acknowledgment checks final.
      const spec = await client.query<{ phase: string; current_semantic_doc_seq: string }>(
        `SELECT phase, current_semantic_doc_seq::text AS current_semantic_doc_seq
           FROM spec WHERE id = $1 FOR UPDATE`,
        [input.specId],
      );
      const row = spec.rows[0];
      if (!row || row.phase !== "drafting") {
        await client.query("ROLLBACK");
        return { kind: "not_requested" };
      }
      const currentSemanticDocSeq = BigInt(row.current_semantic_doc_seq);
      if (currentSemanticDocSeq !== input.semanticDocSeq) {
        await client.query("ROLLBACK");
        return { kind: "stale_document", currentSemanticDocSeq };
      }

      const questions = await client.query<{ id: string }>(
        `SELECT id FROM spec_open_question
          WHERE spec_id = $1 AND state = 'open'
          ORDER BY id`,
        [input.specId],
      );
      const acknowledged = [...input.acknowledgedQuestionIds].sort();
      const open = questions.rows.map((question) => question.id);
      if (
        open.length !== acknowledged.length ||
        open.some((id, index) => id !== acknowledged[index])
      ) {
        await client.query("ROLLBACK");
        return {
          kind: "confirmation_failed",
          reason: "The open questions changed after the acknowledgment.",
        };
      }

      // The checkpoint, the phase flip, and the row advance commit together.
      // A published spec therefore always has its pinned checkpoint, and the
      // document store refuses every later edit.
      await client.query(
        `INSERT INTO spec_checkpoint
           (id, spec_id, state, state_vector, rendered_markdown, doc_seq,
            label, author_user_id, reason, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (id) DO NOTHING`,
        [
          input.checkpoint.id,
          input.specId,
          Buffer.from(input.checkpoint.state),
          Buffer.from(input.checkpoint.stateVector),
          input.checkpoint.renderedMarkdown,
          input.checkpoint.docSeq.toString(),
          input.checkpoint.label,
          input.publishedBy,
          input.checkpoint.reason,
          input.at,
        ],
      );
      const advanced = await client.query(
        `UPDATE spec_publish
            SET state = 'pinned', pinned_at = $2, last_error = NULL
          WHERE spec_id = $1 AND state = 'requested'`,
        [input.specId, input.at],
      );
      if (advanced.rowCount === 0) {
        await client.query("ROLLBACK");
        return { kind: "not_requested" };
      }
      await client.query(
        `UPDATE spec
            SET phase = 'published', published_checkpoint_id = $2,
                published_by = $3, published_at = $4, updated_at = $4
          WHERE id = $1 AND phase = 'drafting'`,
        [input.specId, input.checkpoint.id, input.publishedBy, input.at],
      );
      await client.query("COMMIT");
      return { kind: "pinned" };
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

  async markBlocked(input: { specId: string; reason: string; at: Date }): Promise<boolean> {
    const result = await this.pool.query(
      `UPDATE spec_publish
          SET state = 'blocked', last_error = $2, next_attempt_at = $3
        WHERE spec_id = $1 AND state = 'requested'`,
      [input.specId, input.reason.slice(0, 2000), input.at],
    );
    return (result.rowCount ?? 0) > 0;
  }

  async resetBlocked(input: {
    specId: string;
    requestedBy: string | null;
    requestedAt: Date;
    acknowledgedQuestionIds: readonly string[];
    gapCheckRunId: string | null;
  }): Promise<boolean> {
    const ids = [...input.acknowledgedQuestionIds];
    const result = await this.pool.query(
      `UPDATE spec_publish
          SET state = 'requested', requested_by = $2, requested_at = $3,
              acknowledged_question_ids = $4, acknowledged_question_count = $5,
              gap_check_run_id = $6, attempts = 0, next_attempt_at = $3,
              last_error = NULL
        WHERE spec_id = $1 AND state = 'blocked'`,
      [input.specId, input.requestedBy, input.requestedAt, JSON.stringify(ids), ids.length, input.gapCheckRunId],
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

async function rollbackQuietly(client: { query: (sql: string) => Promise<unknown> }): Promise<void> {
  try {
    await client.query("ROLLBACK");
  } catch {
    // The connection is already broken; the caller's error is the real one.
  }
}
