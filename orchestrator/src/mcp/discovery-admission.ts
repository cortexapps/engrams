import { fromJson, toJson } from "@bufbuild/protobuf";
import { and, eq, gt, inArray, or } from "drizzle-orm";
import type { WorkerIdentity, WorkerIntent } from "../../../mcp-worker/src/contracts.ts";
import { getDb } from "../db/client.ts";
import { integrationConnection, mcpDiscoveryAdmission as admission, user } from "../db/schema.ts";
import { CredentialKind, CredentialStatusSchema } from "../gen/engram/mcp/v1/control_pb.ts";
import { isServiceAccountEmail } from "../rpc/api-key.ts";
import { lockAdmissionScope } from "./admission.ts";
import { validateMcpConfig } from "./config.ts";
import type { AdmittedCredentialOperation } from "./credential-broker.ts";
import { canonicalDigest, canonicalJson } from "./hash.ts";
import { assertSchemaShape } from "./catalog.ts";
import type { JournalTx } from "./journal-review.ts";

export type McpAdministrativeOperation =
  | { kind: "discover" }
  | { kind: "validateSchemas"; documentsDigest: string };
export interface McpSchemaReservation extends Omit<McpDiscoveryReservation, "destination"> {
  documents: Record<string, unknown>[];
}
export interface AdmittedSchemaOperation {
  identity: WorkerIdentity;
  intent: Omit<WorkerIntent, "operationDigest">;
  documents: Record<string, unknown>[];
}
function schemaOperation(documents: Record<string, unknown>[]): McpAdministrativeOperation {
  if (documents.length > 384 || Buffer.byteLength(canonicalJson({ documents })) > 1024 * 1024)
    throw Error("MCP schema limit");
  for (const document of documents) assertSchemaShape(document);
  return { kind: "validateSchemas", documentsDigest: canonicalDigest(documents) };
}
export interface McpDiscoveryReservation {
  requestId: string;
  actorId: string;
  connectionId: string;
  configRevision: number;
  authGeneration: number;
  credentialIdentity: string;
  deploymentId: string;
  identity: WorkerIdentity;
  destination: string;
}
type Row = typeof admission.$inferSelect;
function intentFor(row: Row): Omit<WorkerIntent, "operationDigest"> {
  return {
    id: row.intentId,
    bootId: row.identity.bootId,
    zoneId: row.identity.zoneId,
    policyDigest: row.identity.policyDigest,
    connectionId: row.connectionId,
    connectionRevision: row.configRevision,
    credentialIdentity: row.credentialIdentity,
    scope: { kind: "administration", requestId: row.requestId, actorId: row.actorId },
    operation: row.operation.kind,
    destination: row.destination,
    requestDigest: row.requestDigest,
    expiresAt: row.expiresAt.getTime(),
    lease: { id: row.intentId, deploymentId: row.deploymentId, expiresAt: row.expiresAt.getTime() },
  };
}
/** Uses existing human, connection and credential authority; no credential values are stored. */
export function makeMcpDiscoveryAdmission(db = getDb()) {
  async function live(
    tx: JournalTx,
    input: McpDiscoveryReservation & { operation?: McpAdministrativeOperation },
  ) {
    // Scope -> human -> connection -> discovery row. No lock survives a network operation.
    const [actor] = await tx.select().from(user).where(eq(user.id, input.actorId)).for("share");
    if (!actor || actor.role !== "admin" || actor.banned || isServiceAccountEmail(actor.email))
      throw new Error("human administrator required for discovery");
    const [connection] = await tx
      .select()
      .from(integrationConnection)
      .where(eq(integrationConnection.id, input.connectionId))
      .for("share");
    if (
      !connection ||
      connection.provider !== "mcp" ||
      connection.revoked ||
      connection.mcpCredential.transition ||
      !connection.mcpCredential.status ||
      connection.configRevision !== input.configRevision ||
      connection.authGeneration !== input.authGeneration ||
      connection.credentialIdentity !== input.credentialIdentity
    )
      throw new Error("stale discovery authority");
    const config = validateMcpConfig(connection.config);
    const status = fromJson(CredentialStatusSchema, connection.mcpCredential.status);
    const context = status.context,
      reference = status.reference;
    if (
      status.protocol !== 1 ||
      status.status !== "connected" ||
      !context ||
      !reference ||
      context.connectionId !== input.connectionId ||
      context.configRevision !== BigInt(input.configRevision) ||
      context.authorizationGeneration !== BigInt(input.authGeneration) ||
      context.credentialIdentity !== input.credentialIdentity ||
      context.resource?.endpoint !== config.endpoint ||
      context.resource.zoneId !== input.identity.zoneId ||
      context.policyDigest !== input.identity.policyDigest ||
      context.policyRevision !== input.identity.policyRevision ||
      (input.operation?.kind === "validateSchemas"
        ? input.destination !== ""
        : config.endpoint !== input.destination) ||
      config.zone !== input.identity.zoneId ||
      input.identity.protocol !== 1 ||
      !input.identity.bootId ||
      !input.identity.policyDigest ||
      !input.identity.policyRevision ||
      (config.auth.kind === "api_key"
        ? reference.kind !== CredentialKind.API_KEY ||
          reference.connectionId !== input.connectionId ||
          reference.name !== config.auth.secretRef
        : reference.kind !== CredentialKind.OAUTH ||
          reference.registrationId !== config.auth.clientRegistrationId)
    )
      throw new Error("stale discovery credential context");
    return canonicalDigest(toJson(CredentialStatusSchema, status));
  }
  async function locked(
    requestId: string,
    intentId: string,
    now: Date,
    action: "dispatch" | "finish",
  ) {
    now = new Date(now.getTime());
    const [observed] = await db.select().from(admission).where(eq(admission.requestId, requestId));
    if (!observed) throw new Error("discovery admission unavailable");
    return db.transaction(async (tx) => {
      await lockAdmissionScope(tx, observed.deploymentId);
      if (action === "dispatch") await live(tx, observed);
      const [row] = await tx
        .select()
        .from(admission)
        .where(eq(admission.requestId, requestId))
        .for("update");
      if (
        !row ||
        row.intentId !== intentId ||
        (action === "dispatch" && (row.state !== "reserved" || row.expiresAt <= now))
      )
        throw new Error("discovery intent unavailable");
      await tx
        .update(admission)
        .set({ state: action === "dispatch" ? "dispatched" : "finished" })
        .where(eq(admission.requestId, requestId));
    });
  }
  async function reserve(
    input: McpDiscoveryReservation & { operation: McpAdministrativeOperation },
    now: Date,
  ): Promise<Omit<WorkerIntent, "operationDigest">> {
    input = structuredClone(input);
    now = new Date(now.getTime());
    if (
      ![
        input.requestId,
        input.actorId,
        input.connectionId,
        input.deploymentId,
        input.credentialIdentity,
      ].every(Boolean) ||
      !Number.isSafeInteger(input.configRevision) ||
      input.configRevision < 1 ||
      !Number.isSafeInteger(input.authGeneration) ||
      input.authGeneration < 1 ||
      !Number.isFinite(now.getTime())
    )
      throw new Error("invalid discovery request");
    return db.transaction(async (tx) => {
      await lockAdmissionScope(tx, input.deploymentId);
      const statusDigest = await live(tx, input);
      const [prior] = await tx
        .select()
        .from(admission)
        .where(eq(admission.requestId, input.requestId));
      if (prior) throw new Error("discovery request already consumed");
      // Expired rows retain their identity forever; only their fixed lease stops owning capacity.
      const active = await tx
        .select()
        .from(admission)
        .where(
          and(
            inArray(admission.state, ["reserved", "dispatched"]),
            gt(admission.expiresAt, now),
            or(
              eq(admission.deploymentId, input.deploymentId),
              eq(admission.connectionId, input.connectionId),
            ),
          ),
        );
      if (
        active.filter(
          (row) =>
            row.operation.kind === input.operation.kind && row.deploymentId === input.deploymentId,
        ).length >= 4 ||
        (input.operation.kind === "discover" &&
          active.some(
            (row) => row.operation.kind === "discover" && row.connectionId === input.connectionId,
          ))
      )
        throw new Error("discovery capacity unavailable");
      const row: Row = {
        ...input,
        intentId: crypto.randomUUID(),
        statusDigest,
        requestDigest: canonicalDigest({ ...input, statusDigest }),
        state: "reserved",
        createdAt: now,
        expiresAt: new Date(now.getTime() + 30_000),
      };
      await tx.insert(admission).values(row);
      return intentFor(row);
    });
  }
  async function assertSchemaAdmitted(operation: AdmittedSchemaOperation, now: Date) {
    operation = structuredClone(operation);
    const purpose = schemaOperation(operation.documents),
      scope = operation.intent.scope;
    if (scope.kind !== "administration") throw Error("MCP schema scope required");
    const [observed] = await db
      .select()
      .from(admission)
      .where(eq(admission.requestId, scope.requestId));
    if (!observed) throw Error("MCP schema admission unavailable");
    await db.transaction(async (tx) => {
      await lockAdmissionScope(tx, observed.deploymentId);
      const statusDigest = await live(tx, observed);
      const [row] = await tx
        .select()
        .from(admission)
        .where(eq(admission.requestId, scope.requestId))
        .for("share");
      if (
        !row ||
        row.state !== "dispatched" ||
        row.expiresAt <= now ||
        row.statusDigest !== statusDigest ||
        canonicalDigest(row.operation) !== canonicalDigest(purpose) ||
        canonicalDigest(operation.identity) !== canonicalDigest(row.identity) ||
        canonicalDigest(operation.intent) !== canonicalDigest(intentFor(row))
      )
        throw Error("stale MCP schema admission");
    });
  }
  return {
    reserve: (input: McpDiscoveryReservation, now: Date) =>
      reserve({ ...input, operation: { kind: "discover" } }, now),
    reserveSchema: (input: McpSchemaReservation, now: Date) => {
      const { documents, ...identity } = input;
      return reserve({ ...identity, destination: "", operation: schemaOperation(documents) }, now);
    },
    assertSchemaAdmitted,
    beginDispatch: (requestId: string, intentId: string, now: Date) =>
      locked(requestId, intentId, now, "dispatch"),
    finish: (requestId: string, intentId: string, now: Date) =>
      locked(requestId, intentId, now, "finish"),
    async assertAdmitted(operation: Readonly<AdmittedCredentialOperation>, now: Date) {
      operation = structuredClone(operation);
      now = new Date(now.getTime());
      const scope = operation.intent.scope;
      if (scope.kind !== "administration") throw new Error("discovery scope required");
      const [observed] = await db
        .select()
        .from(admission)
        .where(eq(admission.requestId, scope.requestId));
      if (!observed) throw new Error("discovery admission unavailable");
      await db.transaction(async (tx) => {
        await lockAdmissionScope(tx, observed.deploymentId);
        const statusDigest = await live(tx, observed);
        const [row] = await tx
          .select()
          .from(admission)
          .where(eq(admission.requestId, scope.requestId))
          .for("share");
        if (
          !row ||
          row.state !== "dispatched" ||
          row.operation.kind !== "discover" ||
          row.expiresAt <= now ||
          row.statusDigest !== statusDigest ||
          canonicalDigest(operation.identity) !== canonicalDigest(row.identity) ||
          canonicalDigest(operation.intent) !== canonicalDigest(intentFor(row)) ||
          canonicalDigest(operation.operation) !==
            canonicalDigest({ kind: "discover", endpoint: row.destination }) ||
          canonicalDigest(toJson(CredentialStatusSchema, operation.status)) !== row.statusDigest
        )
          throw new Error("stale discovery admission");
      });
    },
  };
}
