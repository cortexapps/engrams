import { and, eq, sql } from "drizzle-orm";
import { create, fromJson, toJson, type JsonObject } from "@bufbuild/protobuf";
import { getDb } from "../db/client.ts";
import { integrationConnection as connection } from "../db/schema.ts";
import {
  CredentialKind,
  CredentialStatusSchema,
  AuthorityContextSchema,
} from "../gen/engram/mcp/v1/control_pb.ts";
import { validateMcpConfig, type McpConfig } from "./config.ts";

/** Only coordinator metadata. Secret values, callback codes and state never enter this type. */
export interface CredentialMetadata {
  status?: JsonObject;
  clientSecret?: JsonObject;
  flow?: {
    id: string;
    actorId: string;
    registrationId: string;
    context: JsonObject;
    expiresAt: number;
  };
  transition?: { id: string; actorId: string; startedAt: number };
}
export interface CredentialClaim {
  id: string;
  token: string;
  revision: number;
  generation: number;
  metadata: CredentialMetadata;
  config: McpConfig;
}
export function statusMetadata(
  value: ReturnType<typeof create<typeof CredentialStatusSchema>>,
): JsonObject {
  // Protobuf JSON keeps int64 values exact and excludes transport/request fields.
  return toJson(CredentialStatusSchema, value) as JsonObject;
}
function checkedMetadata(value: CredentialMetadata): CredentialMetadata {
  return {
    ...(value.status
      ? { status: statusMetadata(fromJson(CredentialStatusSchema, value.status)) }
      : {}),
    ...(value.clientSecret
      ? { clientSecret: statusMetadata(fromJson(CredentialStatusSchema, value.clientSecret)) }
      : {}),
    ...(value.flow
      ? {
          flow: {
            id: value.flow.id,
            actorId: value.flow.actorId,
            registrationId: value.flow.registrationId,
            expiresAt: value.flow.expiresAt,
            context: toJson(
              AuthorityContextSchema,
              fromJson(AuthorityContextSchema, value.flow.context),
            ) as JsonObject,
          },
        }
      : {}),
    ...(value.transition
      ? {
          transition: {
            id: value.transition.id,
            actorId: value.transition.actorId,
            startedAt: value.transition.startedAt,
          },
        }
      : {}),
  };
}
export function makeCredentialMetadataStore(db = getDb()) {
  async function get(id: string) {
    const [row] = await db
      .select()
      .from(connection)
      .where(and(eq(connection.id, id), eq(connection.provider, "mcp")));
    if (!row) throw new Error("MCP connection not found");
    return row;
  }
  async function claim(
    id: string,
    revision: number,
    generation: number,
    actorId: string,
    config: unknown,
    now: Date,
    callbackId?: string,
  ) {
    const target = validateMcpConfig(config);
    return db.transaction(async (tx) => {
      const [row] = await tx
        .select()
        .from(connection)
        .where(and(eq(connection.id, id), eq(connection.provider, "mcp")))
        .for("update");
      if (
        !row ||
        row.configRevision !== revision ||
        row.authGeneration !== generation ||
        row.mcpCredential.transition
      )
        throw new Error("stale credential transition");
      if (
        callbackId &&
        (row.mcpCredential.flow?.id !== callbackId ||
          row.mcpCredential.flow.actorId !== actorId ||
          row.mcpCredential.flow.expiresAt <= now.getTime())
      )
        throw new Error("stale authorization flow");
      const metadata = checkedMetadata(row.mcpCredential);
      metadata.transition = { id: crypto.randomUUID(), actorId, startedAt: now.getTime() };
      if (!callbackId) delete metadata.flow;
      const nextRevision = revision + (callbackId ? 0 : 1),
        nextGeneration = generation + (callbackId ? 0 : 1);
      await tx
        .update(connection)
        .set({
          mcpCredential: metadata,
          configRevision: nextRevision,
          authGeneration: nextGeneration,
          revoked: true,
          enabled: false,
          testedAt: null,
          updatedAt: now,
        })
        .where(eq(connection.id, id));
      return {
        id,
        token: metadata.transition.id,
        revision: nextRevision,
        generation: nextGeneration,
        metadata,
        config: target,
      };
    });
  }
  async function stage(claim: CredentialClaim, metadata: CredentialMetadata, now: Date) {
    const safe = checkedMetadata({ ...metadata, transition: claim.metadata.transition });
    const [row] = await db
      .update(connection)
      .set({ mcpCredential: safe, updatedAt: now })
      .where(
        and(
          eq(connection.id, claim.id),
          eq(connection.configRevision, claim.revision),
          eq(connection.authGeneration, claim.generation),
          sql`${connection.mcpCredential}->'transition'->>'id' = ${claim.token}`,
        ),
      )
      .returning();
    if (!row) throw new Error("stale credential transition");
    claim.metadata = safe;
  }
  async function publish(
    claim: CredentialClaim,
    config: unknown,
    status: JsonObject | undefined,
    now: Date,
    names?: { alias: string; displayName: string },
  ) {
    const target = validateMcpConfig(config);
    const receipt = status ? fromJson(CredentialStatusSchema, status) : undefined;
    if (
      receipt &&
      (receipt.protocol !== 1 ||
        receipt.status !== "connected" ||
        !receipt.context ||
        receipt.context.connectionId !== claim.id ||
        receipt.context.configRevision !== BigInt(claim.revision) ||
        receipt.context.authorizationGeneration !== BigInt(claim.generation + 1) ||
        receipt.context.resource?.endpoint !== target.endpoint ||
        receipt.context.resource.zoneId !== target.zone ||
        !receipt.context.credentialIdentity ||
        !receipt.reference ||
        (target.auth.kind === "api_key"
          ? receipt.reference.kind !== CredentialKind.API_KEY ||
            receipt.reference.connectionId !== claim.id ||
            receipt.reference.name !== target.auth.secretRef
          : receipt.reference.kind !== CredentialKind.OAUTH ||
            receipt.reference.registrationId !== target.auth.clientRegistrationId))
    )
      throw new Error("stale authority receipt");
    const metadata = checkedMetadata({ ...claim.metadata, ...(status ? { status } : {}) });
    delete metadata.transition;
    if (receipt) delete metadata.flow;
    const [row] = await db
      .update(connection)
      .set({
        ...names,
        config: target,
        mcpCredential: metadata,
        ...(receipt
          ? {
              authGeneration: claim.generation + 1,
              credentialIdentity: receipt.context!.credentialIdentity,
              revoked: false,
            }
          : {}),
        enabled: false,
        testedAt: null,
        updatedAt: now,
      })
      .where(
        and(
          eq(connection.id, claim.id),
          eq(connection.configRevision, claim.revision),
          eq(connection.authGeneration, claim.generation),
          sql`${connection.mcpCredential}->'transition'->>'id' = ${claim.token}`,
        ),
      )
      .returning();
    if (!row) throw new Error("stale credential transition");
    return row;
  }
  return {
    get,
    claim,
    stage,
    publish,
    async remove(claim: CredentialClaim) {
      const rows = await db
        .delete(connection)
        .where(
          and(
            eq(connection.id, claim.id),
            eq(connection.configRevision, claim.revision),
            eq(connection.authGeneration, claim.generation),
            sql`${connection.mcpCredential}->'transition'->>'id' = ${claim.token}`,
          ),
        )
        .returning({ id: connection.id });
      if (!rows.length) throw new Error("stale credential transition");
      return true;
    },
    async recover(id: string, revision: number, generation: number, actorId: string, now: Date) {
      return db.transaction(async (tx) => {
        const [row] = await tx.select().from(connection).where(eq(connection.id, id)).for("update");
        if (
          !row ||
          row.provider !== "mcp" ||
          row.configRevision !== revision ||
          row.authGeneration !== generation ||
          !row.mcpCredential.transition ||
          row.mcpCredential.transition.startedAt + 35000 > now.getTime()
        )
          throw new Error("stale or active credential transition");
        const metadata = checkedMetadata(row.mcpCredential);
        metadata.transition = { id: crypto.randomUUID(), actorId, startedAt: now.getTime() };
        await tx
          .update(connection)
          .set({ mcpCredential: metadata, updatedAt: now })
          .where(eq(connection.id, id));
        return {
          id,
          token: metadata.transition.id,
          revision,
          generation,
          metadata,
          config: validateMcpConfig(row.config),
        };
      });
    },
  };
}
export type CredentialMetadataStore = ReturnType<typeof makeCredentialMetadataStore>;
