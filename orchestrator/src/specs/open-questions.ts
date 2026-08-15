import type { Pool } from "pg";

export type OpenQuestionState = "open" | "resolved";

export interface OpenQuestionRecord {
  id: string;
  specId: string;
  sectionId: string;
  text: string;
  openedBy: string | null;
  requestFingerprint: string;
  state: OpenQuestionState;
  resolutionLink: string | null;
  resolvedBy: string | null;
  resolvedAt: Date | null;
}

export interface CreateOpenQuestionInput {
  id: string;
  specId: string;
  sectionId: string;
  text: string;
  openedBy: string | null;
  requestFingerprint: string;
}

export interface ResolveOpenQuestionInput {
  id: string;
  expectedState: "open";
  resolutionLink: string;
  resolvedBy: string | null;
  resolvedAt: Date;
}

export interface OpenQuestionStore {
  find(id: string): Promise<OpenQuestionRecord | null>;
  countOpenBySection(specId: string): Promise<Record<string, number>>;
  listOpenBySpec(specId: string): Promise<OpenQuestionRecord[]>;
  create(input: CreateOpenQuestionInput): Promise<OpenQuestionRecord>;
  resolve(input: ResolveOpenQuestionInput): Promise<boolean>;
}

export interface QuestionDocument {
  addQuestionMarker(input: {
    questionId: string;
    specId: string;
    sectionId: string;
    anchor: string;
    requestFingerprint: string;
    expectedDocSeq?: bigint;
  }): Promise<boolean>;
  removeQuestionMarker(input: { questionId: string; specId: string }): Promise<void>;
  absorbAnswer(input: {
    questionId: string;
    specId: string;
    sectionId: string;
    answerMarkdown: string;
    expectedDocSeq?: bigint;
  }): Promise<{ changed: boolean; replayed: boolean; resolutionLink: string | null }>;
}

export interface OpenQuestionServiceOptions {
  store: OpenQuestionStore;
  document: QuestionDocument;
  now: () => Date;
}

export class OpenQuestionError extends Error {
  constructor(
    readonly code:
      | "question_text_required"
      | "question_id_required"
      | "question_id_invalid"
      | "question_key_conflict"
      | "answer_required"
      | "question_not_found"
      | "question_not_open"
      | "document_change_required"
      | "stale_question",
    message: string,
  ) {
    super(message);
    this.name = "OpenQuestionError";
  }
}

/** Coordinates the question row with its marker and document resolution. */
export class OpenQuestionService {
  constructor(private readonly options: OpenQuestionServiceOptions) {}

  countOpenBySection(specId: string): Promise<Record<string, number>> {
    return this.options.store.countOpenBySection(specId);
  }

  async open(input: {
    questionId: string;
    specId: string;
    sectionId: string;
    text: string;
    openedBy: string | null;
    anchor: string;
    expectedDocSeq?: bigint;
  }): Promise<OpenQuestionRecord> {
    const text = input.text.trim();
    if (text.length === 0) {
      throw new OpenQuestionError("question_text_required", "An open question must have text.");
    }
    const id = input.questionId.trim();
    if (id.length === 0) {
      throw new OpenQuestionError(
        "question_id_required",
        "An open question must have a stable ID.",
      );
    }
    if (!UUID_PATTERN.test(id)) {
      throw new OpenQuestionError("question_id_invalid", "An open question ID must be a UUID.");
    }
    const requestFingerprint = JSON.stringify([
      input.specId,
      input.sectionId,
      text,
      input.openedBy,
      input.anchor,
      input.expectedDocSeq?.toString() ?? null,
    ]);
    const markerInserted = await this.options.document.addQuestionMarker({
      questionId: id,
      specId: input.specId,
      sectionId: input.sectionId,
      anchor: input.anchor,
      requestFingerprint,
      expectedDocSeq: input.expectedDocSeq,
    });
    try {
      const stored = await this.options.store.create({
        id,
        specId: input.specId,
        sectionId: input.sectionId,
        text,
        openedBy: input.openedBy,
        requestFingerprint,
      });
      assertQuestionRequest(stored, {
        id,
        specId: input.specId,
        sectionId: input.sectionId,
        text,
        openedBy: input.openedBy,
        requestFingerprint,
      });
      return stored;
    } catch (error) {
      let storedAfterError: OpenQuestionRecord | null;
      try {
        storedAfterError = await this.options.store.find(id);
      } catch {
        // Keep the marker when the database commit state is unknown. A retry can
        // use the same stable request key to finish the operation.
        throw error;
      }
      if (storedAfterError) {
        try {
          assertQuestionRequest(storedAfterError, {
            id,
            specId: input.specId,
            sectionId: input.sectionId,
            text,
            openedBy: input.openedBy,
            requestFingerprint,
          });
          return storedAfterError;
        } catch (requestError) {
          if (!markerInserted) throw requestError;
          await removeMarkerAfterProvenFailure(
            this.options.document,
            id,
            input.specId,
            requestError,
          );
          throw requestError;
        }
      }
      if (markerInserted) {
        try {
          await this.options.document.removeQuestionMarker({
            questionId: id,
            specId: input.specId,
          });
        } catch (cleanupError) {
          throw new AggregateError(
            [error, cleanupError],
            "The question row and marker cleanup both failed.",
          );
        }
      }
      throw error;
    }
  }

  /**
   * Close a question without a document answer.
   *
   * Resolution requires the answer in the document; dismissal is the other
   * honest exit — the question no longer applies. It removes the marker and
   * records who dismissed it, so an irrelevant or duplicated question is not
   * carried to publish forever. Dismissing an already-closed question replays.
   */
  async dismiss(input: {
    questionId: string;
    resolvedBy: string | null;
  }): Promise<OpenQuestionRecord> {
    const question = await this.options.store.find(input.questionId);
    if (!question) {
      throw new OpenQuestionError("question_not_found", "The open question does not exist.");
    }
    if (question.state !== "open") return question;
    await this.options.document.removeQuestionMarker({
      questionId: question.id,
      specId: question.specId,
    });
    const resolvedAt = this.options.now();
    const applied = await this.options.store.resolve({
      id: question.id,
      expectedState: "open",
      resolutionLink: DISMISSED_RESOLUTION,
      resolvedBy: input.resolvedBy,
      resolvedAt,
    });
    if (!applied) {
      const latest = await this.options.store.find(question.id);
      if (latest?.state === "resolved") return latest;
      throw new OpenQuestionError(
        "stale_question",
        "The open question changed before the dismissal was stored.",
      );
    }
    return {
      ...question,
      state: "resolved",
      resolutionLink: DISMISSED_RESOLUTION,
      resolvedBy: input.resolvedBy,
      resolvedAt,
    };
  }

  async resolve(input: {
    questionId: string;
    answerMarkdown: string;
    resolvedBy: string | null;
    expectedDocSeq?: bigint;
  }): Promise<OpenQuestionRecord> {
    const answerMarkdown = input.answerMarkdown.trim();
    if (answerMarkdown.length === 0) {
      throw new OpenQuestionError("answer_required", "A question resolution must have an answer.");
    }
    const question = await this.options.store.find(input.questionId);
    if (!question) {
      throw new OpenQuestionError("question_not_found", "The open question does not exist.");
    }
    if (question.state !== "open") {
      throw new OpenQuestionError("question_not_open", "The open question is already resolved.");
    }

    const documentResult = await this.options.document.absorbAnswer({
      questionId: question.id,
      specId: question.specId,
      sectionId: question.sectionId,
      answerMarkdown,
      expectedDocSeq: input.expectedDocSeq,
    });
    if (
      (!documentResult.changed && documentResult.replayed !== true) ||
      documentResult.resolutionLink == null
    ) {
      throw new OpenQuestionError(
        "document_change_required",
        "A question cannot resolve until its answer is in the document.",
      );
    }

    const resolvedAt = this.options.now();
    const applied = await this.options.store.resolve({
      id: question.id,
      expectedState: "open",
      resolutionLink: documentResult.resolutionLink,
      resolvedBy: input.resolvedBy,
      resolvedAt,
    });
    if (!applied) {
      const latest = await this.options.store.find(question.id);
      if (latest?.state === "resolved" && latest.resolutionLink === documentResult.resolutionLink) {
        return latest;
      }
      throw new OpenQuestionError(
        "stale_question",
        "The open question changed before the resolution was stored.",
      );
    }
    return {
      ...question,
      state: "resolved",
      resolutionLink: documentResult.resolutionLink,
      resolvedBy: input.resolvedBy,
      resolvedAt,
    };
  }
}

const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** The resolution note a dismissal records instead of a document anchor. */
export const DISMISSED_RESOLUTION = "dismissed";

async function removeMarkerAfterProvenFailure(
  document: QuestionDocument,
  questionId: string,
  specId: string,
  error: unknown,
): Promise<void> {
  try {
    await document.removeQuestionMarker({ questionId, specId });
  } catch (cleanupError) {
    throw new AggregateError(
      [error, cleanupError],
      "The question request and marker cleanup both failed.",
    );
  }
}

interface OpenQuestionRow {
  id: string;
  spec_id: string;
  section_id: string;
  text: string;
  opened_by: string | null;
  state: OpenQuestionState;
  resolution_note: string | null;
  request_fingerprint: string;
  resolved_by: string | null;
  resolved_at: Date | null;
}

interface OpenQuestionCountRow {
  section_id: string;
  count: string;
}

export class PostgresOpenQuestionStore implements OpenQuestionStore {
  constructor(private readonly pool: Pool) {}

  async find(id: string): Promise<OpenQuestionRecord | null> {
    const result = await this.pool.query<OpenQuestionRow>(
      `SELECT id, spec_id, section_id, text, opened_by, request_fingerprint,
              state, resolution_note, resolved_by, resolved_at
         FROM spec_open_question
        WHERE id = $1`,
      [id],
    );
    const row = result.rows[0];
    return row
      ? {
          id: row.id,
          specId: row.spec_id,
          sectionId: row.section_id,
          text: row.text,
          openedBy: row.opened_by,
          requestFingerprint: row.request_fingerprint,
          state: row.state,
          resolutionLink: row.resolution_note,
          resolvedBy: row.resolved_by,
          resolvedAt: row.resolved_at,
        }
      : null;
  }

  async countOpenBySection(specId: string): Promise<Record<string, number>> {
    const result = await this.pool.query<OpenQuestionCountRow>(
      `SELECT section_id, count(*)::text AS count
         FROM spec_open_question
        WHERE spec_id = $1 AND state = 'open'
        GROUP BY section_id`,
      [specId],
    );
    return Object.fromEntries(result.rows.map((row) => [row.section_id, Number(row.count)]));
  }

  async listOpenBySpec(specId: string): Promise<OpenQuestionRecord[]> {
    const result = await this.pool.query<OpenQuestionRow>(
      `SELECT id, spec_id, section_id, text, opened_by, request_fingerprint,
              state, resolution_note, resolved_by, resolved_at
         FROM spec_open_question
        WHERE spec_id = $1 AND state = 'open'
        ORDER BY created_at, id`,
      [specId],
    );
    return result.rows.map((row) => ({
      id: row.id,
      specId: row.spec_id,
      sectionId: row.section_id,
      text: row.text,
      openedBy: row.opened_by,
      requestFingerprint: row.request_fingerprint,
      state: row.state,
      resolutionLink: row.resolution_note,
      resolvedBy: row.resolved_by,
      resolvedAt: row.resolved_at,
    }));
  }

  async create(input: CreateOpenQuestionInput): Promise<OpenQuestionRecord> {
    await this.pool.query(
      `INSERT INTO spec_open_question
         (id, spec_id, section_id, text, opened_by, request_fingerprint,
          state, resolution_note, resolved_at)
       VALUES ($1, $2, $3, $4, $5, $6, 'open', NULL, NULL)
       ON CONFLICT (id) DO NOTHING`,
      [
        input.id,
        input.specId,
        input.sectionId,
        input.text,
        input.openedBy,
        input.requestFingerprint,
      ],
    );
    const stored = await this.find(input.id);
    if (!stored) throw new Error("The open question was not stored.");
    return stored;
  }

  async resolve(input: ResolveOpenQuestionInput): Promise<boolean> {
    const result = await this.pool.query(
      `UPDATE spec_open_question
          SET state = 'resolved', resolution_note = $2, resolved_by = $3, resolved_at = $4
        WHERE id = $1 AND state = $5`,
      [input.id, input.resolutionLink, input.resolvedBy, input.resolvedAt, input.expectedState],
    );
    return result.rowCount === 1;
  }
}

function assertQuestionRequest(stored: OpenQuestionRecord, input: CreateOpenQuestionInput): void {
  if (
    stored.id !== input.id ||
    stored.specId !== input.specId ||
    stored.sectionId !== input.sectionId ||
    stored.text !== input.text ||
    stored.openedBy !== input.openedBy ||
    stored.requestFingerprint !== input.requestFingerprint
  ) {
    throw new OpenQuestionError(
      "question_key_conflict",
      "The question ID belongs to a different request.",
    );
  }
}
