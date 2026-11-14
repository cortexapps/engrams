import type { IntegrationPolicyJson } from "../connectors/registry.ts";
import type { ProfileIntegrationGrant } from "../db/schema.ts";
import type { SessionCreateInput } from "../rpc/task-create.ts";
import type { TrustedCreationInput } from "./session-create.ts";

export interface McpSessionCapabilities {
  compile(
    input: SessionCreateInput,
    context: {
      profileId: string;
      profileGrants: readonly ProfileIntegrationGrant[];
      grants: readonly ProfileIntegrationGrant[];
      directPolicy: IntegrationPolicyJson;
      spawningSessionId?: string;
    },
  ): Promise<SessionCreateInput>;
  create(input: SessionCreateInput, source: TrustedCreationInput): Promise<{ sessionId: string }>;
}
let installed: McpSessionCapabilities | undefined;
export function installMcpSessionCapabilities(value: McpSessionCapabilities) {
  if (installed) throw Error("MCP session runtime already installed");
  installed = value;
}
export function mcpSessionCapabilities() {
  return installed;
}
