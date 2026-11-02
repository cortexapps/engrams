export class McpPublicationRequired extends Error {
  constructor() {
    super("MCP publication required");
  }
}
export type McpRebindDrain = {
  reason: "dispatch_in_progress" | "unknown_intent" | "legacy_intent";
  retryAfter: string;
};
export class McpRebindDraining extends Error {
  constructor(readonly drain: McpRebindDrain) {
    super("MCP dispatch in progress");
  }
}
