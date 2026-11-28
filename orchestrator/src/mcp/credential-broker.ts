import { createHash } from "node:crypto";
import type {
  WorkerIdentity,
  WorkerIntent,
  WorkerOperation,
  WorkerRequest,
  WorkerResponse,
} from "../../../mcp-worker/src/contracts.ts";
import { canonical, parseWorkerRequest } from "../../../mcp-worker/src/index.ts";
import type { CredentialStatus } from "../gen/engram/mcp/v1/control_pb.ts";
import type { CredentialControlClient } from "./control-client.ts";

export interface McpResourceWorker {
  identity(): Promise<WorkerIdentity>;
  /** Fixed send only. Put awaited policy checks in the broker beforeDispatch hook. */
  dispatch(request: WorkerRequest): Promise<WorkerResponse>;
}
type ResourceOperation = Extract<WorkerOperation, { kind: "execute" | "discover" }>;
export interface AdmittedCredentialOperation {
  status: CredentialStatus;
  identity: WorkerIdentity;
  intent: Omit<WorkerIntent, "operationDigest">;
  operation: ResourceOperation;
}
/** The runtime must check live authority and its durable global admission here. */
export interface McpResourceAdmission {
  assertAdmitted(operation: Readonly<AdmittedCredentialOperation>): Promise<void>;
}
export function makeCredentialBroker(deps: {
  control: CredentialControlClient;
  worker: McpResourceWorker;
  admission: McpResourceAdmission;
  now: () => number;
  /** Final authority fence; receives no credentials and runs before the last delivery-expiry check. */
  beforeDispatch?(): Promise<void>;
}) {
  if (!deps.admission?.assertAdmitted) throw new Error("MCP global admission is required");
  return {
    async dispatch(input: AdmittedCredentialOperation): Promise<WorkerResponse> {
      const frozen = structuredClone(input),
        { status, identity, intent, operation } = frozen;
      const context = status.context,
        reference = status.reference,
        now = deps.now();
      if (
        !context ||
        !reference ||
        status.protocol !== 1 ||
        status.status !== "connected" ||
        context.connectionId !== intent.connectionId ||
        context.configRevision !== BigInt(intent.connectionRevision) ||
        context.credentialIdentity !== intent.credentialIdentity ||
        context.resource?.endpoint !== intent.destination ||
        context.resource.zoneId !== intent.zoneId ||
        identity.protocol !== 1 ||
        identity.bootId !== intent.bootId ||
        identity.zoneId !== intent.zoneId ||
        identity.policyDigest !== intent.policyDigest ||
        context.policyDigest !== identity.policyDigest ||
        context.policyRevision !== identity.policyRevision ||
        operation.endpoint !== intent.destination ||
        operation.kind !== intent.operation ||
        "credential" in operation ||
        intent.expiresAt <= now ||
        intent.expiresAt > now + 300000 ||
        intent.lease.expiresAt < intent.expiresAt ||
        intent.lease.expiresAt > now + 300000
      )
        throw new Error("stale credential dispatch context");
      if (intent.requestDigest.length !== 71 || !/^sha256:[0-9a-f]{64}$/.test(intent.requestDigest))
        throw new Error("MCP request digest invalid");
      parseWorkerRequest({
        protocol: 1,
        intent: { ...intent, operationDigest: "0".repeat(64) },
        operation,
      });
      await deps.admission.assertAdmitted(structuredClone(frozen));
      const actual = await deps.worker.identity();
      if (canonical(actual) !== canonical(identity)) throw new Error("stale worker identity");
      const deliveryExpiry = Math.min(deps.now() + 10000, intent.expiresAt);
      if (deliveryExpiry <= deps.now()) throw new Error("credential delivery expired");
      const lease = await deps.control.lease({
        protocol: 1,
        context,
        reference,
        worker: identity,
        intentId: intent.id,
        requestDigest: intent.requestDigest.slice("sha256:".length),
        operation: operation.kind,
        expiresAt: BigInt(deliveryExpiry),
        operationExpiresAt: BigInt(intent.expiresAt),
        admission: { ...intent.lease, expiresAt: BigInt(intent.lease.expiresAt) },
        scope:
          intent.scope.kind === "execution"
            ? { case: "execution", value: intent.scope }
            : { case: "administration", value: intent.scope },
      });
      if (
        lease.protocol !== 1 ||
        lease.intentId !== intent.id ||
        lease.expiresAt !== BigInt(deliveryExpiry) ||
        lease.operationExpiresAt !== BigInt(intent.expiresAt) ||
        !["authorization", "x-api-key", "api-key"].includes(lease.header) ||
        !lease.value ||
        /[\r\n]/.test(lease.value)
      )
        throw new Error("invalid credential delivery");
      if (deps.now() >= deliveryExpiry) throw new Error("credential delivery expired");
      // parseWorkerRequest validates the credential header union at the wire boundary.
      const withCredential = {
        ...operation,
        credential: { header: lease.header, value: lease.value },
      };
      const operationDigest = createHash("sha256").update(canonical(withCredential)).digest("hex");
      const request = parseWorkerRequest({
        protocol: 1,
        intent: { ...intent, operationDigest },
        operation: withCredential,
      });
      if (deps.now() >= deliveryExpiry) throw new Error("credential delivery expired");
      await deps.admission.assertAdmitted(structuredClone(frozen));
      if (deps.beforeDispatch) await deps.beforeDispatch();
      if (deps.now() >= deliveryExpiry) throw new Error("credential delivery expired");
      let response: WorkerResponse;
      try {
        response = await deps.worker.dispatch(request);
      } catch {
        throw new Error("MCP worker operation unavailable");
      }
      // No retry on transport loss or mismatch. Task 5 records uncertain writes.
      if (
        !response.ok ||
        canonical(response.identity) !== canonical(identity) ||
        canonical(response.intent) !== canonical(request.intent) ||
        response.intentId !== intent.id ||
        response.requestDigest !== intent.requestDigest ||
        response.operationDigest !== operationDigest ||
        response.leaseId !== intent.lease.id
      )
        throw new Error("MCP worker response mismatch");
      return response;
    },
  };
}
