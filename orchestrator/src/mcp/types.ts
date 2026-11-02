import type { ProfileIntegrationGrant } from "../db/schema.ts";
import type { McpConfig } from "./config.ts";
import { assertSchemaShape } from "./catalog.ts";

export interface McpToolGrant {
  upstreamName: string;
  automationWrite: boolean;
  argumentConstraints: Record<string, unknown>;
  /** Required affirmative choice when constraints are empty. */
  unrestricted: boolean;
}
export interface McpConnectionState {
  configRevision: number;
  authGeneration: number;
  credentialIdentity: string;
  catalogRevision: number;
  revoked: boolean;
}
export interface McpDiscoveredTool {
  upstreamName: string;
  description: string;
  inputSchema: Record<string, unknown>;
  outputSchema?: Record<string, unknown>;
}
export interface McpReviewedTool extends McpDiscoveredTool {
  classification: "read" | "write";
  enabled: boolean;
  fingerprint: string;
}
export interface McpBoundTool {
  name: string;
  tool: McpReviewedTool;
  grant: McpToolGrant;
}
export interface McpBinding extends McpConnectionState {
  connectionId: string;
  config: McpConfig;
  tools: McpBoundTool[];
  policyHash: string;
  hash: string;
}
export type McpExecutionState =
  | "waiting_approval"
  | "authorized"
  | "dispatching"
  | "succeeded"
  | "failed"
  | "canceled"
  | "outcome_unknown";

export function validateMcpGrant(grant: ProfileIntegrationGrant): void {
  if (
    grant.operation !== "invoke" ||
    grant.credentialScope !== "org" ||
    grant.resourceConstraints.length !== 0 ||
    !grant.mcp
  ) {
    throw new Error("MCP grants require invoke, explicit org credentials, and a tool grant");
  }
  const tool = grant.mcp;
  if (
    !tool.upstreamName ||
    tool.upstreamName.length > 256 ||
    /[\u0000-\u001f]/.test(tool.upstreamName) ||
    typeof tool.automationWrite !== "boolean" ||
    typeof tool.unrestricted !== "boolean"
  )
    throw new Error("invalid MCP tool grant");
  assertSchemaShape(tool.argumentConstraints);
  const empty = Object.keys(tool.argumentConstraints).length === 0;
  if (empty !== tool.unrestricted)
    throw new Error("empty constraints require an explicit unrestricted choice");
}
