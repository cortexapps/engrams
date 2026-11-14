// This wire contract has no application-store or runtime dependencies.
export const WORKER_PROTOCOL = 1;
export const PROTOCOL_VERSIONS = [
  "2026-07-28",
  "2025-11-25",
  "2025-06-18",
] as const;
export type JsonObject = Record<string, unknown>;
export interface ZoneConfig {
  id: string;
  revision: string;
  mode: "public" | "internal";
  authorities: string[];
  aliases: string[];
  cidrs: string[];
  denyCidrs: string[];
  ca?: string | undefined;
  limits?: {
    execute?: number;
    discover?: number;
    oauth?: number;
    schema?: number;
    toolMs?: number;
  };
}
export interface WorkerIdentity {
  protocol: 1;
  bootId: string;
  zoneId: string;
  policyDigest: string;
  policyRevision: string;
}
export interface ProviderCredential {
  header: "authorization" | "x-api-key" | "api-key";
  value: string;
}
export interface UpstreamTool {
  name: string;
  description?: string;
  inputSchema: JsonObject;
  outputSchema?: JsonObject;
  annotations?: JsonObject;
}
export interface ToolOutput {
  content: { type: "text"; text: string }[];
  structuredContent?: unknown;
  isError?: boolean;
}
export interface ExecutionResult {
  outcome: "succeeded" | "failed" | "outcome_unknown";
  delivery: "available" | "unavailable";
  output?: ToolOutput;
  reason?:
    | "unsupported_content"
    | "result_limit"
    | "output_schema"
    | "upstream_failure";
}
export type WorkerOperation =
  | { kind: "discover"; endpoint: string; credential?: ProviderCredential }
  | {
      kind: "execute";
      endpoint: string;
      tool: string;
      arguments: JsonObject;
      inputSchema: JsonObject;
      outputSchema?: JsonObject;
      argumentConstraintDocuments: JsonObject[];
      classification: "read" | "write";
      credential?: ProviderCredential;
    }
  | { kind: "validateSchemas"; documents: JsonObject[]; value?: unknown }
  | { kind: "validateDestination"; endpoint: string }
  | { kind: "metadata"; endpoint: string }
  | {
      kind: "token";
      endpoint: string;
      grant:
        | {
            grant_type: "authorization_code";
            code: string;
            redirect_uri: string;
            code_verifier: string;
            client_id: string;
            resource: string;
            client_secret?: string;
          }
        | {
            grant_type: "refresh_token";
            refresh_token: string;
            client_id: string;
            resource: string;
            client_secret?: string;
          };
    }
  | { kind: "cancel"; targetIntentId: string };
// A trusted caller creates this only after global durable admission. The worker
// checks its bindings but has no database access to acquire or renew that lease.
export type WorkerScope =
  | {
      kind: "execution";
      sessionId: string;
      runId: string;
      toolCallId: string;
      bindingHash: string;
    }
  | { kind: "administration"; requestId: string; actorId: string };
export interface WorkerIntent {
  id: string;
  bootId: string;
  zoneId: string;
  policyDigest: string;
  connectionId: string;
  connectionRevision: number;
  credentialIdentity: string;
  scope: WorkerScope;
  operation: WorkerOperation["kind"];
  destination: string;
  requestDigest: string;
  operationDigest: string;
  expiresAt: number;
  lease: { id: string; deploymentId: string; expiresAt: number };
}
export interface WorkerRequest {
  protocol: 1;
  intent: WorkerIntent;
  operation: WorkerOperation;
}
export type WorkerResponse =
  | {
      ok: true;
      identity: WorkerIdentity;
      intent: WorkerIntent;
      intentId: string;
      requestDigest: string;
      operationDigest: string;
      leaseId: string;
      result: unknown;
    }
  | { ok: false; error: string };
