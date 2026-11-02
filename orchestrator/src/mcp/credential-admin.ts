import { Code, ConnectError } from "@connectrpc/connect";
import { create, equals, fromJson, toJson, type JsonObject } from "@bufbuild/protobuf";
import {
  AuthorityContextSchema,
  CredentialKind,
  CredentialStatusSchema,
  type CredentialStatus,
  type AuthorityContext,
} from "../gen/engram/mcp/v1/control_pb.ts";
import type { CredentialControlClient } from "./control-client.ts";
import {
  type CredentialMetadataStore,
  type CredentialClaim,
  statusMetadata,
} from "./credential-store.ts";
import { validateMcpConfig, type McpConfig } from "./config.ts";

export interface McpRegistrationMetadata {
  id: string;
  clientId: string;
  confidential: boolean;
  redirectUri: string;
  issuer: string;
  resource: { zoneId: string; endpoint: string };
  scopes: string[];
}
function decode(value: JsonObject | undefined) {
  return value ? fromJson(CredentialStatusSchema, value) : undefined;
}
function sameAuthority(a: CredentialStatus, b: CredentialStatus) {
  return (
    a.protocol === 1 &&
    b.protocol === 1 &&
    a.context &&
    b.context &&
    a.reference &&
    b.reference &&
    equals(AuthorityContextSchema, a.context, b.context) &&
    a.reference.kind === b.reference.kind &&
    a.reference.connectionId === b.reference.connectionId &&
    a.reference.name === b.reference.name &&
    a.reference.registrationId === b.reference.registrationId
  );
}
function revokedReceipt(old: CredentialStatus, receipt: CredentialStatus) {
  if (
    !old.context ||
    !receipt.context ||
    !old.reference ||
    !receipt.reference ||
    receipt.protocol !== 1 ||
    receipt.status !== "revoked" ||
    receipt.version !== old.version + 1n ||
    receipt.reference.kind !== old.reference.kind ||
    receipt.reference.name !== old.reference.name ||
    receipt.reference.registrationId !== old.reference.registrationId ||
    receipt.reference.connectionId !== old.reference.connectionId
  )
    throw new Error("stale revocation receipt");
  if (old.reference.kind === CredentialKind.OAUTH) {
    if (!equals(AuthorityContextSchema, old.context, receipt.context))
      throw new Error("stale revocation receipt");
  } else if (
    receipt.reference.generation !== old.reference.generation + 1n ||
    receipt.context.authorizationGeneration !== old.context.authorizationGeneration + 1n ||
    receipt.context.credentialIdentity === old.context.credentialIdentity ||
    !equals(
      AuthorityContextSchema,
      create(AuthorityContextSchema, {
        ...receipt.context,
        authorizationGeneration: old.context.authorizationGeneration,
        credentialIdentity: old.context.credentialIdentity,
      }),
      old.context,
    )
  )
    throw new Error("stale revocation receipt");
}
export function makeCredentialAdmin(deps: {
  store: CredentialMetadataStore;
  control: CredentialControlClient;
  registrations: readonly McpRegistrationMetadata[];
  now: () => Date;
}) {
  const { store, control, now } = deps;
  // Copy only operator metadata. No secret-bearing operator object is retained here.
  const registrations = deps.registrations.map((r) => ({
    id: r.id,
    clientId: r.clientId,
    confidential: r.confidential,
    redirectUri: r.redirectUri,
    issuer: r.issuer,
    resource: { ...r.resource },
    scopes: [...r.scopes],
  }));
  function registration(config: McpConfig) {
    if (config.auth.kind !== "oauth") throw new Error("OAuth connection required");
    const registrationId = config.auth.clientRegistrationId;
    const r = registrations.find((r) => r.id === registrationId);
    if (!r || r.resource.zoneId !== config.zone || r.resource.endpoint !== config.endpoint)
      throw new Error("approved registration required");
    return r;
  }
  function change(claim: CredentialClaim) {
    return {
      connectionId: claim.id,
      configRevision: BigInt(claim.revision),
      previousAuthorizationGeneration: BigInt(claim.generation),
      resource: { zoneId: claim.config.zone, endpoint: claim.config.endpoint },
    };
  }
  async function revoke(claim: CredentialClaim, field: "status" | "clientSecret", actorId: string) {
    const saved = decode(claim.metadata[field]);
    if (!saved) return;
    const current = await control.getStatus({
      protocol: 1,
      connectionId: claim.id,
      reference: saved.reference,
    });
    if (saved.status === "revoked") {
      if (!equals(CredentialStatusSchema, current, saved))
        throw new Error("stale revoked authority");
      return current;
    }
    // A lost reply can be recovered from the durable receipt, never by replay.
    if (current.status === "revoked") {
      revokedReceipt(saved, current);
      await store.stage(claim, { ...claim.metadata, [field]: statusMetadata(current) }, now());
      return current;
    }
    if (!sameAuthority(saved, current)) throw new Error("stale credential authority");
    // Normal refresh may change version/status while its identity stays fixed.
    await store.stage(claim, { ...claim.metadata, [field]: statusMetadata(current) }, now());
    let receipt: CredentialStatus;
    try {
      receipt = await control.revoke({ protocol: 1, expected: current, actorId });
    } catch {
      receipt = await control.getStatus({
        protocol: 1,
        connectionId: claim.id,
        reference: current.reference,
      });
    }
    revokedReceipt(current, receipt);
    await store.stage(claim, { ...claim.metadata, [field]: statusMetadata(receipt) }, now());
    return receipt;
  }
  async function revokeTargetOAuth(
    claim: CredentialClaim,
    actorId: string,
    recover = false,
    cancelledContext?: AuthorityContext,
  ) {
    if (claim.config.auth.kind !== "oauth") return undefined;
    const registrationId = claim.config.auth.clientRegistrationId;
    const saved = decode(claim.metadata.status);
    if (
      !recover &&
      saved?.reference?.kind === CredentialKind.OAUTH &&
      saved.reference.registrationId === registrationId
    )
      return revoke(claim, "status", actorId);
    let current: CredentialStatus;
    try {
      current = await control.getStatus({
        protocol: 1,
        connectionId: claim.id,
        reference: { kind: CredentialKind.OAUTH, registrationId },
      });
    } catch (error) {
      if (
        error instanceof ConnectError &&
        error.code === Code.NotFound &&
        (!saved ||
          saved.reference?.kind !== CredentialKind.OAUTH ||
          saved.reference.registrationId !== registrationId)
      )
        return undefined;
      throw error;
    }
    if (
      current.context?.connectionId !== claim.id ||
      current.reference?.kind !== CredentialKind.OAUTH ||
      current.reference.registrationId !== registrationId
    )
      throw new Error("stale OAuth authority");
    if (
      recover &&
      !(
        (saved?.context &&
          saved.reference?.kind === CredentialKind.OAUTH &&
          saved.reference.registrationId === registrationId &&
          equals(AuthorityContextSchema, current.context!, saved.context)) ||
        (cancelledContext && equals(AuthorityContextSchema, current.context!, cancelledContext))
      )
    )
      throw new Error("stale OAuth recovery authority");
    // Accept only the saved authority or this canceled attempt's exact context.
    // A newer same-registration authority is not evidence for old recovery.
    await store.stage(claim, { ...claim.metadata, status: statusMetadata(current) }, now());
    return revoke(claim, "status", actorId);
  }
  async function cancelPending(claim: CredentialClaim, actorId: string) {
    const flow = claim.metadata.flow;
    if (!flow) return;
    const receipt = await control.cancelAuthorization({
      protocol: 1,
      flowId: flow.id,
      connectionId: claim.id,
      registrationId: flow.registrationId,
      actorId,
    });
    if (
      receipt.protocol !== 1 ||
      receipt.flowId !== flow.id ||
      receipt.connectionId !== claim.id ||
      receipt.registrationId !== flow.registrationId
    )
      throw new Error("stale cancellation receipt");
    // Keep the attempt proof durable until the whole transition finishes.
    // A later recovery may need it to identify a callback that won publication.
    return flow;
  }
  async function claimCurrent(
    id: string,
    revision: number,
    generation: number,
    actor: string,
    config?: McpConfig,
  ) {
    const row = await store.get(id);
    const claim = await store.claim(id, revision, generation, actor, config ?? row.config, now());
    await cancelPending(claim, actor);
    delete claim.metadata.flow;
    return claim;
  }
  return {
    registrations: () => structuredClone(registrations),
    async status(id: string) {
      const row = await store.get(id),
        stored = decode(row.mcpCredential.status);
      const status = stored
        ? await control.getStatus({ protocol: 1, connectionId: id, reference: stored.reference })
        : undefined;
      return { row, status };
    },
    async putSecret(
      id: string,
      revision: number,
      generation: number,
      actorId: string,
      kind: CredentialKind,
      header: string,
      value: string,
    ) {
      if (
        !value ||
        value.length > 8192 ||
        /[\r\n]/.test(value) ||
        !["authorization", "x-api-key", "api-key"].includes(header)
      )
        throw new Error("invalid credential input");
      const row = await store.get(id),
        config = validateMcpConfig(row.config);
      if (
        kind === CredentialKind.API_KEY
          ? config.auth.kind !== "api_key"
          : kind !== CredentialKind.CLIENT_SECRET || !registration(config).confidential
      )
        throw new Error("credential kind does not match connection");
      const claim = await claimCurrent(id, revision, generation, actorId);
      const resource = await revoke(claim, "status", actorId);
      const old =
        kind === CredentialKind.CLIENT_SECRET
          ? await revoke(claim, "clientSecret", actorId)
          : resource;
      const expected =
        old?.reference?.kind === kind &&
        old.context?.authorizationGeneration === BigInt(claim.generation)
          ? old.reference
          : undefined;
      const result = await control.putSecret({
        protocol: 1,
        context: change(claim),
        kind,
        expected,
        header,
        value,
        actorId,
      });
      // Retain the returned reference before publication, so a crash can revoke it.
      const field = kind === CredentialKind.API_KEY ? "status" : "clientSecret";
      await store.stage(claim, { ...claim.metadata, [field]: statusMetadata(result) }, now());
      if (kind === CredentialKind.API_KEY) {
        if (result.reference?.kind !== CredentialKind.API_KEY || !result.reference.name)
          throw new Error("invalid credential receipt");
        await store.publish(
          claim,
          { ...config, auth: { kind: "api_key", secretRef: result.reference.name } },
          statusMetadata(result),
          now(),
        );
      } else {
        if (
          result.reference?.kind !== CredentialKind.CLIENT_SECRET ||
          result.context?.connectionId !== id ||
          result.context.configRevision !== BigInt(claim.revision) ||
          result.context.authorizationGeneration !== BigInt(claim.generation + 1)
        )
          throw new Error("invalid client credential receipt");
        await store.publish(claim, config, undefined, now());
      }
    },
    async begin(
      id: string,
      revision: number,
      generation: number,
      actorId: string,
      consent: boolean,
    ) {
      const row = await store.get(id),
        config = validateMcpConfig(row.config),
        r = registration(config);
      if (!consent || config.auth.kind !== "oauth" || !config.auth.shareWithOrgSessions)
        throw new Error("organization sharing consent required");
      const claim = await claimCurrent(id, revision, generation, actorId);
      await revoke(claim, "status", actorId);
      const old = await revokeTargetOAuth(claim, actorId);
      const client = decode(claim.metadata.clientSecret);
      if (
        r.confidential &&
        (!client ||
          client.status !== "connected" ||
          client.reference?.kind !== CredentialKind.CLIENT_SECRET)
      )
        throw new Error("client credential required");
      const response = await control.beginAuthorization({
        protocol: 1,
        context: change(claim),
        registrationId: r.id,
        clientSecret: r.confidential ? client!.reference : undefined,
        expectedVersion: old?.reference?.registrationId === r.id ? old.version : undefined,
        actorId,
        sharingConsent: consent,
      });
      if (
        response.protocol !== 1 ||
        !response.context ||
        response.context.connectionId !== id ||
        response.context.configRevision !== BigInt(claim.revision) ||
        response.context.authorizationGeneration !== BigInt(claim.generation + 1) ||
        response.expiresAt <= BigInt(now().getTime()) ||
        response.expiresAt > BigInt(now().getTime() + 600000)
      )
        throw new Error("invalid authorization receipt");
      const url = new URL(response.authorizationUrl);
      if (url.protocol !== "https:" || url.username || url.password || url.hash)
        throw new Error("invalid authorization URL");
      await store.stage(
        claim,
        {
          ...claim.metadata,
          flow: {
            id: response.flowId,
            actorId,
            registrationId: r.id,
            context: toJson(AuthorityContextSchema, response.context) as JsonObject,
            expiresAt: Number(response.expiresAt),
          },
        },
        now(),
      );
      await store.publish(claim, config, undefined, now());
      return {
        configRevision: claim.revision,
        authGeneration: claim.generation,
        authorizationUrl: response.authorizationUrl,
        flowId: response.flowId,
        expiresAt: new Date(Number(response.expiresAt)).toISOString(),
      };
    },
    async complete(
      id: string,
      revision: number,
      generation: number,
      actorId: string,
      input: { flowId: string; state: string; code: string; issuer?: string; redirectUri: string },
    ) {
      const row = await store.get(id),
        config = validateMcpConfig(row.config),
        r = registration(config);
      if (
        input.redirectUri !== r.redirectUri ||
        (input.issuer !== undefined && input.issuer !== r.issuer)
      )
        throw new Error("invalid authorization callback");
      const claim = await store.claim(
        id,
        revision,
        generation,
        actorId,
        config,
        now(),
        input.flowId,
      );
      const flow = claim.metadata.flow!;
      if (flow.registrationId !== r.id) throw new Error("stale authorization registration");
      const result = await control.completeAuthorization({
        protocol: 1,
        context: fromJson(AuthorityContextSchema, flow.context),
        registrationId: r.id,
        flowId: flow.id,
        state: input.state,
        code: input.code,
        issuer: input.issuer,
        redirectUri: r.redirectUri,
        actorId,
      });
      await store.stage(claim, { ...claim.metadata, status: statusMetadata(result) }, now());
      await store.publish(claim, config, statusMetadata(result), now());
    },
    async disconnect(id: string, revision: number, generation: number, actorId: string) {
      const claim = await claimCurrent(id, revision, generation, actorId);
      await revoke(claim, "status", actorId);
      await store.publish(claim, claim.config, undefined, now());
    },
    async configure(
      id: string,
      revision: number,
      generation: number,
      actorId: string,
      config: McpConfig,
      names?: { alias: string; displayName: string },
    ) {
      if (config.auth.kind === "oauth") registration(config);
      const row = await store.get(id),
        current = validateMcpConfig(row.config);
      if (
        config.auth.kind === "api_key" &&
        config.auth.secretRef &&
        (current.auth.kind !== "api_key" || config.auth.secretRef !== current.auth.secretRef)
      )
        throw new Error("fresh credential ingestion required");
      const claim = await claimCurrent(id, revision, generation, actorId, config);
      await revoke(claim, "status", actorId);
      await revoke(claim, "clientSecret", actorId);
      await store.publish(claim, config, undefined, now(), names);
    },
    async remove(id: string, revision: number, generation: number, actorId: string) {
      const claim = await claimCurrent(id, revision, generation, actorId);
      await revoke(claim, "status", actorId);
      await revoke(claim, "clientSecret", actorId);
      return store.remove(claim);
    },
    async recover(id: string, revision: number, generation: number, actorId: string) {
      const claim = await store.recover(id, revision, generation, actorId, now());
      const cancelled = await cancelPending(claim, actorId);
      const cancelledContext = cancelled
        ? fromJson(AuthorityContextSchema, cancelled.context)
        : undefined;
      if (
        claim.config.auth.kind === "oauth" &&
        decode(claim.metadata.status)?.reference?.kind === CredentialKind.OAUTH
      )
        await revokeTargetOAuth(claim, actorId, true, cancelledContext);
      else {
        await revoke(claim, "status", actorId);
        await revokeTargetOAuth(claim, actorId, true, cancelledContext);
      }
      // Client-secret writes also need a durable fence after interrupted publication.
      await revoke(claim, "clientSecret", actorId);
      delete claim.metadata.flow;
      await store.publish(claim, claim.config, undefined, now());
    },
  };
}
export type CredentialAdmin = ReturnType<typeof makeCredentialAdmin>;
