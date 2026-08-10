import { and, eq, isNull, or, sql } from "drizzle-orm";
import type { NodePgDatabase } from "drizzle-orm/node-postgres";
import * as decoding from "lib0/decoding";
import * as encoding from "lib0/encoding";

import * as schema from "../db/schema.ts";
import { specParticipant } from "../db/schema.ts";
import type { SpecAwarenessBus, SpecParticipantStore } from "../routes/spec-sync.ts";
import {
  encodeSpecChannelEnvelope,
  parseSpecChannelEnvelope,
  SPEC_UPDATE_CHANNEL,
} from "./doc-service.ts";
import {
  listenForSpecChannel,
  type SpecChannelClient,
  type SpecChannelListenerOptions,
} from "./channel-listener.ts";

interface AwarenessPool {
  connect(): Promise<SpecChannelClient>;
  query(queryText: string, values: unknown[]): Promise<unknown>;
}

export class PostgresSpecParticipantStore implements SpecParticipantStore {
  constructor(
    private readonly db: NodePgDatabase<typeof schema>,
    private readonly now: () => Date = () => new Date(),
    private readonly leaseDurationMs = 60_000,
  ) {}

  async connect(specId: string, clientId: string, userId: string): Promise<bigint> {
    const connectedAt = this.now();
    const leaseExpiresAt = new Date(connectedAt.getTime() + this.leaseDurationMs);
    const rows = await this.db
      .insert(specParticipant)
      .values({
        specId,
        clientId,
        userId,
        connectionEpoch: 1n,
        connectedAt,
        disconnectedAt: null,
        leaseExpiresAt,
      })
      .onConflictDoUpdate({
        target: [specParticipant.specId, specParticipant.clientId],
        set: {
          userId,
          connectionEpoch: sql`${specParticipant.connectionEpoch} + 1`,
          connectedAt,
          disconnectedAt: null,
          leaseExpiresAt,
        },
        setWhere: or(isNull(specParticipant.userId), eq(specParticipant.userId, userId)),
      })
      .returning({ userId: specParticipant.userId, epoch: specParticipant.connectionEpoch });
    if (rows.length === 0) {
      throw new Error(`Spec client ${clientId} is already bound to another user`);
    }
    return rows[0]!.epoch;
  }

  async renew(specId: string, clientId: string, epoch: bigint): Promise<boolean> {
    const renewedAt = this.now();
    const rows = await this.db
      .update(specParticipant)
      .set({ leaseExpiresAt: new Date(renewedAt.getTime() + this.leaseDurationMs) })
      .where(
        and(
          eq(specParticipant.specId, specId),
          eq(specParticipant.clientId, clientId),
          eq(specParticipant.connectionEpoch, epoch),
          isNull(specParticipant.disconnectedAt),
        ),
      )
      .returning({ epoch: specParticipant.connectionEpoch });
    return rows.length === 1;
  }

  async disconnect(specId: string, clientId: string, epoch: bigint): Promise<void> {
    const disconnectedAt = this.now();
    await this.db
      .update(specParticipant)
      .set({
        disconnectedAt,
        // This tombstone keeps the current write distinct from a legacy disconnect.
        leaseExpiresAt: sql`'-infinity'::timestamptz`,
      })
      .where(
        and(
          eq(specParticipant.specId, specId),
          eq(specParticipant.clientId, clientId),
          eq(specParticipant.connectionEpoch, epoch),
        ),
      );
  }
}

/** Relay ephemeral awareness on the durable update channel without storing it. */
export class PostgresSpecAwarenessBus implements SpecAwarenessBus {
  private stopListening: (() => Promise<void>) | null = null;

  constructor(
    private readonly pool: AwarenessPool,
    private readonly listenerOptions: SpecChannelListenerOptions = {},
  ) {}

  async start(handlers: {
    update(specId: string, update: Uint8Array): void;
    query(specId: string): void;
    participantConnected(specId: string, clientId: string, epoch: bigint): void;
    reconnect?(): void;
  }): Promise<() => Promise<void>> {
    if (this.stopListening) throw new Error("The spec awareness bus is already started");
    const stop = await listenForSpecChannel(
      this.pool,
      SPEC_UPDATE_CHANNEL,
      (message) => {
        if (message.channel !== SPEC_UPDATE_CHANNEL || !message.payload) return;
        const envelope = parseSpecChannelEnvelope(message.payload);
        if (envelope?.type === "awareness") {
          const update = Buffer.from(envelope.update, "base64");
          if (update.toString("base64") === envelope.update) {
            handlers.update(envelope.specId, update);
          }
        } else if (envelope?.type === "awareness-query") {
          handlers.query(envelope.specId);
        } else if (envelope?.type === "participant-connected") {
          handlers.participantConnected(
            envelope.specId,
            envelope.clientId,
            BigInt(envelope.epoch),
          );
        }
      },
      {
        ...this.listenerOptions,
        label: this.listenerOptions.label ?? "Spec awareness listener",
        onReconnect: () => {
          handlers.reconnect?.();
          this.listenerOptions.onReconnect?.();
        },
      },
    );
    this.stopListening = stop;
    return async () => {
      if (this.stopListening !== stop) return;
      this.stopListening = null;
      await stop();
    };
  }

  async publish(specId: string, update: Uint8Array): Promise<void> {
    for (const chunk of chunkAwarenessUpdate(specId, update)) {
      await this.notify({
        type: "awareness",
        specId,
        update: Buffer.from(chunk).toString("base64"),
      });
    }
  }

  async query(specId: string): Promise<void> {
    await this.notify({ type: "awareness-query", specId });
  }

  async publishParticipantConnected(
    specId: string,
    clientId: string,
    epoch: bigint,
  ): Promise<void> {
    await this.notify({
      type: "participant-connected",
      specId,
      clientId,
      epoch: epoch.toString(),
    });
  }

  private async notify(
    envelope:
      | { type: "awareness"; specId: string; update: string }
      | { type: "awareness-query"; specId: string }
      | { type: "participant-connected"; specId: string; clientId: string; epoch: string },
  ): Promise<void> {
    const payload = encodeSpecChannelEnvelope(envelope);
    await this.pool.query("SELECT pg_notify($1, $2)", [SPEC_UPDATE_CHANNEL, payload]);
  }
}

interface AwarenessRecord {
  clientId: number;
  clock: number;
  state: string;
}

/** Split an aggregate awareness update into PostgreSQL-safe envelopes. */
export function chunkAwarenessUpdate(specId: string, update: Uint8Array): Uint8Array[] {
  const decoder = decoding.createDecoder(update);
  const count = decoding.readVarUint(decoder);
  const records: AwarenessRecord[] = [];
  for (let index = 0; index < count; index += 1) {
    records.push({
      clientId: decoding.readVarUint(decoder),
      clock: decoding.readVarUint(decoder),
      state: decoding.readVarString(decoder),
    });
  }
  if (decoding.hasContent(decoder)) throw new Error("The awareness update has trailing bytes");
  if (records.length === 0) return [update];

  const chunks: Uint8Array[] = [];
  let current: AwarenessRecord[] = [];
  for (const record of records) {
    const candidate = encodeAwarenessRecords([...current, record]);
    if (fitsAwarenessEnvelope(specId, candidate)) {
      current.push(record);
      continue;
    }
    if (current.length === 0) {
      throw new Error("One awareness state is too large for the PostgreSQL channel");
    }
    chunks.push(encodeAwarenessRecords(current));
    current = [record];
    const single = encodeAwarenessRecords(current);
    if (!fitsAwarenessEnvelope(specId, single)) {
      throw new Error("One awareness state is too large for the PostgreSQL channel");
    }
  }
  if (current.length > 0) chunks.push(encodeAwarenessRecords(current));
  return chunks;
}

function encodeAwarenessRecords(records: AwarenessRecord[]): Uint8Array {
  const encoder = encoding.createEncoder();
  encoding.writeVarUint(encoder, records.length);
  for (const record of records) {
    encoding.writeVarUint(encoder, record.clientId);
    encoding.writeVarUint(encoder, record.clock);
    encoding.writeVarString(encoder, record.state);
  }
  return encoding.toUint8Array(encoder);
}

function fitsAwarenessEnvelope(specId: string, update: Uint8Array): boolean {
  try {
    encodeSpecChannelEnvelope({
      type: "awareness",
      specId,
      update: Buffer.from(update).toString("base64"),
    });
    return true;
  } catch {
    return false;
  }
}
