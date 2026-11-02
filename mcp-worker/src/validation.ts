import { object } from "./encoding.ts";
import type {
  WorkerIntent,
  WorkerOperation,
  WorkerRequest,
  WorkerScope,
} from "./contracts.ts";
function text(value: unknown): value is string {
  return typeof value === "string" && value.length > 0 && value.length <= 8192;
}
function schema(value: unknown): value is Record<string, unknown> {
  return object(value);
}
function schemas(value: unknown): value is Record<string, unknown>[] {
  return Array.isArray(value) && value.length <= 384 && value.every(schema);
}
function operationValid(value: unknown): value is WorkerOperation {
  if (!object(value) || !text(value.kind)) return false;
  if (
    "credential" in value &&
    (!object(value.credential) ||
      typeof value.credential.header !== "string" ||
      !["authorization", "api-key", "x-api-key"].includes(
        String(value.credential.header),
      ) ||
      !text(value.credential.value) ||
      /[\r\n]/.test(value.credential.value))
  )
    return false;
  if (
    [
      "discover",
      "execute",
      "metadata",
      "token",
      "validateDestination",
    ].includes(value.kind) &&
    !text(value.endpoint)
  )
    return false;
  switch (value.kind) {
    case "discover":
    case "metadata":
    case "validateDestination":
      return true;
    case "cancel":
      return text(value.targetIntentId);
    case "validateSchemas":
      return (
        schemas(value.documents) &&
        Object.keys(value).every((key) =>
          ["kind", "documents", "value"].includes(key),
        )
      );
    case "execute":
      return (
        text(value.tool) &&
        object(value.arguments) &&
        schema(value.inputSchema) &&
        (value.outputSchema === undefined || schema(value.outputSchema)) &&
        schemas(value.argumentConstraintDocuments) &&
        (value.classification === "read" || value.classification === "write")
      );
    case "token": {
      const g = value.grant;
      if (
        !object(g) ||
        !text(g.client_id) ||
        !text(g.resource) ||
        (g.client_secret !== undefined && !text(g.client_secret))
      )
        return false;
      const base = ["grant_type", "client_id", "resource", "client_secret"];
      if (g.grant_type === "authorization_code")
        return (
          text(g.code) &&
          text(g.redirect_uri) &&
          text(g.code_verifier) &&
          Object.keys(g).every((k) =>
            [...base, "code", "redirect_uri", "code_verifier"].includes(k),
          )
        );
      if (g.grant_type === "refresh_token")
        return (
          text(g.refresh_token) &&
          Object.keys(g).every((k) => [...base, "refresh_token"].includes(k))
        );
      return false;
    }
    default:
      return false;
  }
}
function intentValid(i: unknown): i is WorkerIntent {
  if (!object(i)) return false;
  for (const key of [
    "id",
    "bootId",
    "zoneId",
    "policyDigest",
    "connectionId",
    "credentialIdentity",
    "operation",
    "requestDigest",
    "operationDigest",
  ])
    if (!text(i[key])) return false;
  return (
    scopeValid(i.scope) &&
    typeof i.destination === "string" &&
    Number.isSafeInteger(i.connectionRevision) &&
    Number(i.connectionRevision) >= 1 &&
    Number.isSafeInteger(i.expiresAt) &&
    object(i.lease) &&
    text(i.lease.id) &&
    text(i.lease.deploymentId) &&
    Number.isSafeInteger(i.lease.expiresAt)
  );
}
export function parseWorkerRequest(value: unknown): WorkerRequest {
  if (
    !object(value) ||
    value.protocol !== 1 ||
    !intentValid(value.intent) ||
    !operationValid(value.operation)
  )
    throw Error("invalid_request");
  if (value.intent.operation !== value.operation.kind)
    throw Error("operation_mismatch");
  if (
    value.operation.kind === "execute" &&
    value.intent.scope.kind !== "execution"
  )
    throw Error("execution_scope_required");
  if (
    !["execute", "cancel", "validateSchemas"].includes(value.operation.kind) &&
    value.intent.scope.kind !== "administration"
  )
    throw Error("administration_scope_required");
  return { protocol: 1, intent: value.intent, operation: value.operation };
}

function scopeValid(value: unknown): value is WorkerScope {
  if (!object(value)) return false;
  if (value.kind === "execution")
    return (
      text(value.sessionId) &&
      text(value.runId) &&
      text(value.toolCallId) &&
      typeof value.bindingHash === "string" &&
      /^[a-f0-9]{64}$/.test(value.bindingHash)
    );
  return (
    value.kind === "administration" &&
    text(value.requestId) &&
    text(value.actorId)
  );
}
