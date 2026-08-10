import type { Pool, PoolClient } from "pg";

import {
  applyHumanSectionEdit,
  applySectionStateUndo,
  transitionSectionState,
  type RestoreSectionStateUndo,
  type SectionState,
  type SectionStateChange,
  type SectionStateContext,
  type SectionStateTranscriptChip,
  type SectionStateValue,
} from "./section-state.ts";

const EMPTY_SECTION_STATE: SectionStateValue = { state: "empty", naReason: null };

export interface SectionStateTranscriptAction {
  id: string;
  specId: string;
  sectionId: string;
  requestFingerprint: string;
  chip: SectionStateTranscriptChip;
  createdAt: Date;
  deliveredAt: Date | null;
}

export interface PersistSectionStateActionInput {
  actionId: string;
  specId: string;
  sectionId: string;
  requestFingerprint: string;
  expectedDocSeq?: bigint;
  expected: SectionStateValue;
  next: SectionStateValue;
  confirmedBy: string | null;
  chip: SectionStateTranscriptChip;
  at: Date;
}

export type PersistSectionStateActionResult =
  | { status: "stored" | "replayed"; action: SectionStateTranscriptAction }
  | { status: "conflict" };

export interface SectionStateStore {
  read(specId: string, sectionId: string): Promise<SectionStateValue>;
  readAction(actionId: string): Promise<SectionStateTranscriptAction | null>;
  persistStateAction(
    input: PersistSectionStateActionInput,
  ): Promise<PersistSectionStateActionResult>;
  listPendingActions(limit: number): Promise<SectionStateTranscriptAction[]>;
  markActionDelivered(actionId: string, deliveredAt: Date): Promise<boolean>;
}

export class SectionStateConflictError extends Error {
  constructor(message = "The section state changed before this action was stored.") {
    super(message);
    this.name = "SectionStateConflictError";
  }
}

export interface SectionStateTranscriptPublisher {
  /** Publish at most one visible chip for each stable action ID. */
  publish(actionId: string, chip: SectionStateTranscriptChip): Promise<void>;
}

export interface SectionStateServiceOptions {
  store: SectionStateStore;
  transcript: SectionStateTranscriptPublisher;
  now: () => Date;
}

/** Stores each state change and its transcript action in one transaction. */
export class SectionStateService {
  constructor(private readonly options: SectionStateServiceOptions) {}

  async transition(input: {
    actionId: string;
    context: SectionStateContext;
    target: SectionState;
    naReason?: string | null;
    actorUserId: string | null;
    expectedDocSeq?: bigint;
  }): Promise<SectionStateChange> {
    const change = await this.execute(
      input.actionId,
      fingerprint({
        kind: "transition",
        target: input.target,
        naReason: input.naReason?.trim() || null,
        actorUserId: input.actorUserId,
        expectedDocSeq: input.expectedDocSeq?.toString() ?? null,
      }),
      input.context,
      input.actorUserId,
      input.expectedDocSeq,
      (current) => transitionSectionState(current, input.target, input.context, input.naReason),
    );
    if (!change) throw new Error("A section transition did not change the state.");
    return change;
  }

  /** Records an accepted document edit with the edit event's stable action ID. */
  recordHumanEdit(input: {
    actionId: string;
    context: SectionStateContext;
    actorUserId: string;
    expectedDocSeq?: bigint;
  }): Promise<SectionStateChange | null> {
    return this.execute(
      input.actionId,
      humanEditRequestFingerprint(input.actorUserId, input.expectedDocSeq),
      input.context,
      input.actorUserId,
      input.expectedDocSeq,
      (current) => applyHumanSectionEdit(current, input.context),
    );
  }

  async undo(input: {
    actionId: string;
    context: SectionStateContext;
    undo: RestoreSectionStateUndo;
    actorUserId: string;
    expectedDocSeq?: bigint;
  }): Promise<SectionStateChange> {
    const change = await this.execute(
      input.actionId,
      fingerprint({
        kind: "undo",
        undo: input.undo,
        actorUserId: input.actorUserId,
        expectedDocSeq: input.expectedDocSeq?.toString() ?? null,
      }),
      input.context,
      input.actorUserId,
      input.expectedDocSeq,
      (current) => applySectionStateUndo(current, input.undo, input.context),
    );
    if (!change) throw new Error("A section undo did not change the state.");
    return change;
  }

  private async execute(
    actionId: string,
    requestFingerprint: string,
    context: SectionStateContext,
    actorUserId: string | null,
    expectedDocSeq: bigint | undefined,
    makeChange: (current: SectionStateValue) => SectionStateChange | null,
  ): Promise<SectionStateChange | null> {
    const existing = await this.options.store.readAction(actionId);
    if (existing) {
      this.assertActionContext(existing, context, requestFingerprint);
      await this.deliver(existing);
      return changeFromAction(existing);
    }

    const current = await this.options.store.read(context.specId, context.sectionId);
    const change = makeChange(current);
    if (!change) return null;
    const result = await this.options.store.persistStateAction({
      actionId,
      specId: context.specId,
      sectionId: context.sectionId,
      requestFingerprint,
      expectedDocSeq,
      expected: current,
      next: change.value,
      confirmedBy: change.value.state === "confirmed" ? actorUserId : null,
      chip: change.transcriptChip,
      at: this.options.now(),
    });
    if (result.status === "conflict") throw new SectionStateConflictError();
    this.assertActionContext(result.action, context, requestFingerprint);
    await this.deliver(result.action);
    return changeFromAction(result.action);
  }

  private assertActionContext(
    action: SectionStateTranscriptAction,
    context: SectionStateContext,
    requestFingerprint: string,
  ): void {
    if (action.specId !== context.specId || action.sectionId !== context.sectionId) {
      throw new SectionStateConflictError("The action ID belongs to a different section.");
    }
    if (action.requestFingerprint !== requestFingerprint) {
      throw new SectionStateConflictError("The action ID belongs to a different command.");
    }
  }

  private async deliver(action: SectionStateTranscriptAction): Promise<void> {
    if (action.deliveredAt) return;
    await this.options.transcript.publish(action.id, action.chip);
    await this.options.store.markActionDelivered(action.id, this.options.now());
  }
}

export class SectionStateTranscriptDrainer {
  constructor(
    private readonly store: SectionStateStore,
    private readonly publisher: SectionStateTranscriptPublisher,
    private readonly now: () => Date,
  ) {}

  async runOnce(limit = 100): Promise<{ delivered: number; failed: number }> {
    const actions = await this.store.listPendingActions(limit);
    let delivered = 0;
    let failed = 0;
    for (const action of actions) {
      try {
        await this.publisher.publish(action.id, action.chip);
        if (await this.store.markActionDelivered(action.id, this.now())) delivered += 1;
      } catch {
        failed += 1;
      }
    }
    return { delivered, failed };
  }
}

interface SectionStateRow {
  state: SectionState;
  na_reason: string | null;
}

interface TranscriptActionRow {
  id: string;
  spec_id: string;
  section_id: string;
  request_fingerprint: string;
  chip: SectionStateTranscriptChip;
  created_at: Date;
  delivered_at: Date | null;
}

export class PostgresSectionStateStore implements SectionStateStore {
  constructor(private readonly pool: Pool) {}

  async read(specId: string, sectionId: string): Promise<SectionStateValue> {
    const result = await this.pool.query<SectionStateRow>(
      `SELECT state, na_reason
         FROM spec_section_state
        WHERE spec_id = $1 AND section_id = $2`,
      [specId, sectionId],
    );
    return rowValue(result.rows[0]);
  }

  async readAction(actionId: string): Promise<SectionStateTranscriptAction | null> {
    const result = await this.pool.query<TranscriptActionRow>(
      `SELECT id, spec_id, section_id, request_fingerprint, chip, created_at, delivered_at
         FROM spec_transcript_action
        WHERE id = $1`,
      [actionId],
    );
    return actionValue(result.rows[0]);
  }

  async persistStateAction(
    input: PersistSectionStateActionInput,
  ): Promise<PersistSectionStateActionResult> {
    const client = await this.pool.connect();
    try {
      await client.query("BEGIN");
      const currentDocSeq = await lockSpec(client, input.specId);
      const existing = await readActionWith(client, input.actionId);
      if (existing) {
        await client.query("COMMIT");
        return { status: "replayed", action: existing };
      }
      if (input.expectedDocSeq !== undefined && currentDocSeq !== input.expectedDocSeq) {
        await client.query("ROLLBACK");
        return { status: "conflict" };
      }
      const currentResult = await client.query<SectionStateRow>(
        `SELECT state, na_reason
           FROM spec_section_state
          WHERE spec_id = $1 AND section_id = $2
          FOR UPDATE`,
        [input.specId, input.sectionId],
      );
      if (!sameState(rowValue(currentResult.rows[0]), input.expected)) {
        await client.query("ROLLBACK");
        return { status: "conflict" };
      }
      await client.query(
        `INSERT INTO spec_section_state
           (spec_id, section_id, state, na_reason, confirmed_by, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (spec_id, section_id) DO UPDATE
         SET state = excluded.state,
             na_reason = excluded.na_reason,
             confirmed_by = excluded.confirmed_by,
             updated_at = excluded.updated_at`,
        [
          input.specId,
          input.sectionId,
          input.next.state,
          input.next.naReason,
          input.confirmedBy,
          input.at,
        ],
      );
      const insertedAction = await client.query(
        `INSERT INTO spec_transcript_action
           (id, spec_id, section_id, request_fingerprint, chip, created_at, delivered_at)
         VALUES ($1, $2, $3, $4, $5, $6, NULL)
         ON CONFLICT (id) DO NOTHING
         RETURNING id`,
        [
          input.actionId,
          input.specId,
          input.sectionId,
          input.requestFingerprint,
          input.chip,
          input.at,
        ],
      );
      if (insertedAction.rowCount !== 1) {
        await client.query("ROLLBACK");
        const existing = await readActionWith(client, input.actionId);
        if (!existing) throw new Error("The concurrent transcript action is missing.");
        return { status: "replayed", action: existing };
      }
      await client.query("COMMIT");
      return {
        status: "stored",
        action: {
          id: input.actionId,
          specId: input.specId,
          sectionId: input.sectionId,
          requestFingerprint: input.requestFingerprint,
          chip: input.chip,
          createdAt: input.at,
          deliveredAt: null,
        },
      };
    } catch (error) {
      await client.query("ROLLBACK").catch(() => {});
      throw error;
    } finally {
      client.release();
    }
  }

  async listPendingActions(limit: number): Promise<SectionStateTranscriptAction[]> {
    const result = await this.pool.query<TranscriptActionRow>(
      `SELECT id, spec_id, section_id, request_fingerprint, chip, created_at, delivered_at
         FROM spec_transcript_action
        WHERE delivered_at IS NULL
        ORDER BY created_at, id
        LIMIT $1`,
      [limit],
    );
    return result.rows.map((row) => actionValue(row)!);
  }

  async markActionDelivered(actionId: string, deliveredAt: Date): Promise<boolean> {
    const result = await this.pool.query(
      `UPDATE spec_transcript_action
          SET delivered_at = $2
        WHERE id = $1 AND delivered_at IS NULL`,
      [actionId, deliveredAt],
    );
    return result.rowCount === 1;
  }
}

function actionValue(row: TranscriptActionRow | undefined): SectionStateTranscriptAction | null {
  return row
    ? {
        id: row.id,
        specId: row.spec_id,
        sectionId: row.section_id,
        requestFingerprint: row.request_fingerprint,
        chip: row.chip,
        createdAt: row.created_at,
        deliveredAt: row.delivered_at,
      }
    : null;
}

function changeFromAction(action: SectionStateTranscriptAction): SectionStateChange {
  return { value: action.chip.after, transcriptChip: action.chip };
}

function rowValue(row: SectionStateRow | undefined): SectionStateValue {
  return row ? { state: row.state, naReason: row.na_reason } : EMPTY_SECTION_STATE;
}

function sameState(left: SectionStateValue, right: SectionStateValue): boolean {
  return left.state === right.state && left.naReason === right.naReason;
}

async function lockSpec(client: PoolClient, specId: string): Promise<bigint> {
  const result = await client.query<{ current_doc_seq: string }>(
    "SELECT current_doc_seq::text AS current_doc_seq FROM spec WHERE id = $1 FOR UPDATE",
    [specId],
  );
  if (result.rowCount !== 1) throw new Error("The spec does not exist.");
  return BigInt(result.rows[0]!.current_doc_seq);
}

async function readActionWith(
  client: PoolClient,
  actionId: string,
): Promise<SectionStateTranscriptAction | null> {
  const result = await client.query<TranscriptActionRow>(
    `SELECT id, spec_id, section_id, request_fingerprint, chip, created_at, delivered_at
       FROM spec_transcript_action
      WHERE id = $1`,
    [actionId],
  );
  return actionValue(result.rows[0]);
}

function fingerprint(value: object): string {
  return JSON.stringify(canonicalValue(value));
}

export function humanEditRequestFingerprint(actorUserId: string, expectedDocSeq?: bigint): string {
  return fingerprint({
    kind: "human_edit",
    actorUserId,
    expectedDocSeq: expectedDocSeq?.toString() ?? null,
  });
}

function canonicalValue(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonicalValue);
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((key) => [key, canonicalValue(Reflect.get(value, key))]),
    );
  }
  return value;
}
