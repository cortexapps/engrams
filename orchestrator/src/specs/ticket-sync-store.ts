/**
 * Postgres for the Linear sync (ADR 0114 D6, N4).
 *
 * Two tables and one narrow view of a third. The ledger
 * (`spec_ticket_sync_operation`) is the durable half of N4: a reservation is
 * inserted before any call to Linear, and it is what a driver reads after a pod
 * roll to learn what an earlier attempt already did.
 *
 * The sync writes only the three sync columns of `spec_ticket_draft`. It never
 * goes through the tree's `mutate`, which rewrites the whole tree — a person
 * dragging a row while a batch runs must not lose the drag, and a batch must
 * not lose a row's Linear identity to a concurrent drag.
 */

import type { Pool } from "pg";

import type { SpecTicketSyncState } from "@engrams/spec-document";
import type { SpecTicketSyncOperationStatus } from "../db/schema.ts";
import type {
  ReserveInput,
  SpecTicketSyncStore,
  SpecTicketSyncTarget,
  SyncOperationRow,
  SyncTicketRow,
} from "./ticket-sync.ts";

interface TicketRow {
  id: string;
  parent_id: string | null;
  ordinal: number;
  title: string;
  description: string;
  depends_on: string[];
  sync_state: SpecTicketSyncState;
  linear_id: string | null;
  sync_error: string | null;
}

interface OperationRow {
  caller_spec_id: string;
  operation: string;
  idempotency_key: string;
  request_hash: string;
  status: SpecTicketSyncOperationStatus;
  reserved_ticket_id: string | null;
  reserved_external_id: string | null;
  attempts: number;
  result: Record<string, unknown> | null;
  error: string | null;
}

interface ConfigRow {
  team_id: string | null;
  team_name: string | null;
  project_id: string | null;
  project_name: string | null;
  label_ids: string[];
  label_names: string[];
}

const TICKET_COLUMNS = `id, parent_id, ordinal, title, description, depends_on,
       sync_state, linear_id, sync_error`;

const OPERATION_COLUMNS = `caller_spec_id, operation, idempotency_key, request_hash,
       status, reserved_ticket_id, reserved_external_id, attempts, result, error`;

function ticketRecord(row: TicketRow): SyncTicketRow {
  return {
    id: row.id,
    parentId: row.parent_id,
    ordinal: row.ordinal,
    title: row.title,
    description: row.description,
    dependsOn: row.depends_on,
    syncState: row.sync_state,
    linearId: row.linear_id,
    syncError: row.sync_error,
  };
}

function operationRecord(row: OperationRow): SyncOperationRow {
  return {
    callerSpecId: row.caller_spec_id,
    operation: row.operation,
    idempotencyKey: row.idempotency_key,
    requestHash: row.request_hash,
    status: row.status,
    reservedTicketId: row.reserved_ticket_id,
    reservedExternalId: row.reserved_external_id,
    attempts: row.attempts,
    result: row.result,
    error: row.error,
  };
}

export class PostgresSpecTicketSyncStore implements SpecTicketSyncStore {
  constructor(private readonly pool: Pool) {}

  async listTickets(specId: string): Promise<SyncTicketRow[]> {
    const result = await this.pool.query<TicketRow>(
      `SELECT ${TICKET_COLUMNS} FROM spec_ticket_draft WHERE spec_id = $1 ORDER BY ordinal, id`,
      [specId],
    );
    return result.rows.map(ticketRecord);
  }

  async readTicket(specId: string, ticketId: string): Promise<SyncTicketRow | null> {
    const result = await this.pool.query<TicketRow>(
      `SELECT ${TICKET_COLUMNS} FROM spec_ticket_draft WHERE spec_id = $1 AND id = $2`,
      [specId, ticketId],
    );
    const row = result.rows[0];
    return row ? ticketRecord(row) : null;
  }

  async writeTicketState(
    specId: string,
    ticketId: string,
    patch: { syncState: SpecTicketSyncState; linearId?: string | null; syncError?: string | null },
  ): Promise<void> {
    // COALESCE keeps a column an absent field did not mean to touch. A retry
    // that fails must not erase the Linear identity of an earlier success.
    await this.pool.query(
      `UPDATE spec_ticket_draft
          SET sync_state = $3,
              linear_id = CASE WHEN $4::boolean THEN $5 ELSE linear_id END,
              sync_error = CASE WHEN $6::boolean THEN $7 ELSE sync_error END
        WHERE spec_id = $1 AND id = $2`,
      [
        specId,
        ticketId,
        patch.syncState,
        patch.linearId !== undefined,
        patch.linearId ?? null,
        patch.syncError !== undefined,
        patch.syncError ?? null,
      ],
    );
  }

  async readConfig(specId: string): Promise<SpecTicketSyncTarget | null> {
    const result = await this.pool.query<ConfigRow>(
      `SELECT team_id, team_name, project_id, project_name, label_ids, label_names
         FROM spec_ticket_sync_config WHERE spec_id = $1`,
      [specId],
    );
    const row = result.rows[0];
    if (!row) return null;
    return {
      teamId: row.team_id,
      teamName: row.team_name,
      projectId: row.project_id,
      projectName: row.project_name,
      labelIds: row.label_ids,
      labelNames: row.label_names,
    };
  }

  async writeConfig(specId: string, target: SpecTicketSyncTarget): Promise<void> {
    await this.pool.query(
      `INSERT INTO spec_ticket_sync_config
         (spec_id, team_id, team_name, project_id, project_name, label_ids, label_names)
       VALUES ($1, $2, $3, $4, $5, $6, $7)
       ON CONFLICT (spec_id) DO UPDATE SET
         team_id = EXCLUDED.team_id,
         team_name = EXCLUDED.team_name,
         project_id = EXCLUDED.project_id,
         project_name = EXCLUDED.project_name,
         label_ids = EXCLUDED.label_ids,
         label_names = EXCLUDED.label_names,
         updated_at = now()`,
      [
        specId,
        target.teamId,
        target.teamName,
        target.projectId,
        target.projectName,
        JSON.stringify(target.labelIds),
        JSON.stringify(target.labelNames),
      ],
    );
  }

  /**
   * Insert the reservation, then read what stands.
   *
   * `ON CONFLICT DO NOTHING` is the whole concurrency story: two drivers racing
   * on one ticket both read the same row, and the loser sees the winner's hash
   * and reserved id rather than its own.
   */
  async reserve(input: ReserveInput): Promise<SyncOperationRow> {
    await this.pool.query(
      `INSERT INTO spec_ticket_sync_operation
         (caller_spec_id, operation, idempotency_key, request_hash,
          reserved_ticket_id, reserved_external_id)
       VALUES ($1, $2, $3, $4, $5, $6)
       ON CONFLICT (caller_spec_id, operation, idempotency_key) DO NOTHING`,
      [
        input.specId,
        input.operation,
        input.idempotencyKey,
        input.requestHash,
        input.reservedTicketId,
        input.reservedExternalId,
      ],
    );
    const row = await this.readOperation(input.specId, input.operation, input.idempotencyKey);
    if (!row) throw new Error("spec ticket sync reservation disappeared");
    return row;
  }

  async rehash(
    specId: string,
    operation: string,
    key: string,
    requestHash: string,
  ): Promise<void> {
    await this.pool.query(
      // Any unfinished reservation may be re-bound; a complete one may not.
      // Its arguments are the record of a create that really happened.
      `UPDATE spec_ticket_sync_operation
          SET request_hash = $4, status = 'reserved', error = NULL, updated_at = now()
        WHERE caller_spec_id = $1 AND operation = $2 AND idempotency_key = $3
          AND status <> 'complete'`,
      [specId, operation, key, requestHash],
    );
  }

  async countAttempt(specId: string, operation: string, key: string): Promise<number> {
    const result = await this.pool.query<{ attempts: number }>(
      `UPDATE spec_ticket_sync_operation
          SET attempts = attempts + 1, updated_at = now()
        WHERE caller_spec_id = $1 AND operation = $2 AND idempotency_key = $3
       RETURNING attempts`,
      [specId, operation, key],
    );
    return result.rows[0]?.attempts ?? 0;
  }

  async complete(
    specId: string,
    operation: string,
    key: string,
    result: Record<string, unknown>,
  ): Promise<void> {
    await this.pool.query(
      `UPDATE spec_ticket_sync_operation
          SET status = 'complete', result = $4, error = NULL, updated_at = now()
        WHERE caller_spec_id = $1 AND operation = $2 AND idempotency_key = $3`,
      [specId, operation, key, JSON.stringify(result)],
    );
  }

  /**
   * A complete reservation is never downgraded.
   *
   * `complete` means Linear holds the issue and this ledger row names it. If a
   * later step fails — the state write, the next read, anything — the row must
   * keep saying so, because a row that says `failed` about an issue that exists
   * is the one state from which a retry can create a duplicate. The `WHERE`
   * clause is that guarantee, not a caller's discipline.
   */
  async fail(specId: string, operation: string, key: string, error: string): Promise<void> {
    await this.pool.query(
      `UPDATE spec_ticket_sync_operation
          SET status = 'failed', error = $4, updated_at = now()
        WHERE caller_spec_id = $1 AND operation = $2 AND idempotency_key = $3
          AND status <> 'complete'`,
      [specId, operation, key, error],
    );
  }

  async listOperations(specId: string): Promise<SyncOperationRow[]> {
    const result = await this.pool.query<OperationRow>(
      `SELECT ${OPERATION_COLUMNS} FROM spec_ticket_sync_operation
        WHERE caller_spec_id = $1 ORDER BY operation, idempotency_key`,
      [specId],
    );
    return result.rows.map(operationRecord);
  }

  private async readOperation(
    specId: string,
    operation: string,
    key: string,
  ): Promise<SyncOperationRow | null> {
    const result = await this.pool.query<OperationRow>(
      `SELECT ${OPERATION_COLUMNS} FROM spec_ticket_sync_operation
        WHERE caller_spec_id = $1 AND operation = $2 AND idempotency_key = $3`,
      [specId, operation, key],
    );
    const row = result.rows[0];
    return row ? operationRecord(row) : null;
  }
}
