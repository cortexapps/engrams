import { and, eq, isNull, or } from "drizzle-orm";
import type { NodePgDatabase } from "drizzle-orm/node-postgres";

import * as schema from "../db/schema.ts";
import { specParticipant } from "../db/schema.ts";
import type { SpecAwarenessBus, SpecParticipantStore } from "../routes/spec-sync.ts";
import {
  encodeSpecChannelEnvelope,
  parseSpecChannelEnvelope,
  SPEC_UPDATE_CHANNEL,
} from "./doc-service.ts";

interface AwarenessClient {
  on(
    event: "notification",
    listener: (message: { channel: string; payload?: string }) => void,
  ): void;
  off(
    event: "notification",
    listener: (message: { channel: string; payload?: string }) => void,
  ): void;
  query(queryText: string): Promise<unknown>;
  release(): void;
}

interface AwarenessPool {
  connect(): Promise<AwarenessClient>;
  query(queryText: string, values: unknown[]): Promise<unknown>;
}

export class PostgresSpecParticipantStore implements SpecParticipantStore {
  constructor(
    private readonly db: NodePgDatabase<typeof schema>,
    private readonly now: () => Date = () => new Date(),
  ) {}

  async connect(specId: string, clientId: string, userId: string): Promise<void> {
    const connectedAt = this.now();
    const rows = await this.db
      .insert(specParticipant)
      .values({ specId, clientId, userId, connectedAt, disconnectedAt: null })
      .onConflictDoUpdate({
        target: [specParticipant.specId, specParticipant.clientId],
        set: { userId, connectedAt, disconnectedAt: null },
        setWhere: or(isNull(specParticipant.userId), eq(specParticipant.userId, userId)),
      })
      .returning({ userId: specParticipant.userId });
    if (rows.length === 0) {
      throw new Error(`Spec client ${clientId} is already bound to another user`);
    }
  }

  async disconnect(specId: string, clientId: string): Promise<void> {
    await this.db
      .update(specParticipant)
      .set({ disconnectedAt: this.now() })
      .where(and(eq(specParticipant.specId, specId), eq(specParticipant.clientId, clientId)));
  }
}

/** Relay ephemeral awareness on the durable update channel without storing it. */
export class PostgresSpecAwarenessBus implements SpecAwarenessBus {
  private client: AwarenessClient | null = null;

  constructor(private readonly pool: AwarenessPool) {}

  async start(handlers: {
    update(specId: string, update: Uint8Array): void;
    query(specId: string): void;
  }): Promise<() => Promise<void>> {
    if (this.client) throw new Error("The spec awareness bus is already started");
    const client = await this.pool.connect();
    const onNotification = (message: { channel: string; payload?: string }) => {
      if (message.channel !== SPEC_UPDATE_CHANNEL || !message.payload) return;
      const envelope = parseSpecChannelEnvelope(message.payload);
      if (envelope?.type === "awareness") {
        const update = Buffer.from(envelope.update, "base64");
        if (update.toString("base64") === envelope.update) handlers.update(envelope.specId, update);
      } else if (envelope?.type === "awareness-query") {
        handlers.query(envelope.specId);
      }
    };
    try {
      client.on("notification", onNotification);
      await client.query(`LISTEN ${SPEC_UPDATE_CHANNEL}`);
      this.client = client;
    } catch (error) {
      client.off("notification", onNotification);
      client.release();
      throw error;
    }
    return async () => {
      if (this.client !== client) return;
      this.client = null;
      client.off("notification", onNotification);
      await client.query(`UNLISTEN ${SPEC_UPDATE_CHANNEL}`).catch(() => {});
      client.release();
    };
  }

  async publish(specId: string, update: Uint8Array): Promise<void> {
    await this.notify({
      type: "awareness",
      specId,
      update: Buffer.from(update).toString("base64"),
    });
  }

  async query(specId: string): Promise<void> {
    await this.notify({ type: "awareness-query", specId });
  }

  private async notify(
    envelope:
      | { type: "awareness"; specId: string; update: string }
      | { type: "awareness-query"; specId: string },
  ): Promise<void> {
    const payload = encodeSpecChannelEnvelope(envelope);
    await this.pool.query("SELECT pg_notify($1, $2)", [SPEC_UPDATE_CHANNEL, payload]);
  }
}
