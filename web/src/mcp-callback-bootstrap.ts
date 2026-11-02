export const MCP_CALLBACK_PATH = "/integrations/mcp/callback";
const FLOW_KEY = "engram.mcp.authorization";

export type McpFlow = {
  connectionId: string;
  flowId: string;
  configRevision: number;
  authGeneration: number;
  redirectUri: string;
  expiresAt: string;
};

export function captureMcpCallback(url: URL, history: Pick<History, "replaceState">) {
  if (url.pathname !== MCP_CALLBACK_PATH) return undefined;
  const params = url.searchParams;
  history.replaceState(null, "", MCP_CALLBACK_PATH);
  if (
    params.getAll("code").length !== 1 ||
    params.getAll("state").length !== 1 ||
    params.getAll("iss").length > 1
  )
    return null;
  const code = params.get("code");
  const state = params.get("state");
  if (!code || !state) return null;
  return { code, state, issuer: params.get("iss") ?? undefined };
}

function validFlow(value: unknown, redirectUri: string, now: number): value is McpFlow {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const item = value as Record<string, unknown>;
  const keys = [
    "connectionId",
    "flowId",
    "configRevision",
    "authGeneration",
    "redirectUri",
    "expiresAt",
  ];
  return (
    Object.keys(item).length === keys.length &&
    keys.every((key) => key in item) &&
    typeof item.connectionId === "string" &&
    item.connectionId.length > 0 &&
    typeof item.flowId === "string" &&
    item.flowId.length > 0 &&
    typeof item.configRevision === "number" &&
    Number.isSafeInteger(item.configRevision) &&
    item.configRevision >= 0 &&
    typeof item.authGeneration === "number" &&
    Number.isSafeInteger(item.authGeneration) &&
    item.authGeneration >= 0 &&
    item.redirectUri === redirectUri &&
    typeof item.expiresAt === "string" &&
    Date.parse(item.expiresAt) > now
  );
}

export function assertMcpFlowAvailable(storage: Pick<Storage, "getItem">) {
  if (storage.getItem(FLOW_KEY) !== null)
    throw new Error("An authorization is already pending in this tab.");
}

export function saveMcpFlow(storage: Pick<Storage, "getItem" | "setItem">, flow: McpFlow) {
  assertMcpFlowAvailable(storage);
  const { connectionId, flowId, configRevision, authGeneration, redirectUri, expiresAt } = flow;
  storage.setItem(
    FLOW_KEY,
    JSON.stringify({
      connectionId,
      flowId,
      configRevision,
      authGeneration,
      redirectUri,
      expiresAt,
    }),
  );
}

export function consumeMcpFlow(
  storage: Pick<Storage, "getItem" | "removeItem">,
  redirectUri: string,
  now = Date.now(),
): McpFlow | null {
  try {
    const raw = storage.getItem(FLOW_KEY);
    storage.removeItem(FLOW_KEY);
    if (!raw) return null;
    const value: unknown = JSON.parse(raw);
    return validFlow(value, redirectUri, now) ? value : null;
  } catch {
    return null;
  }
}

export function clearMcpFlowForConnection(
  storage: Pick<Storage, "getItem" | "removeItem">,
  connectionId: string,
) {
  const raw = storage.getItem(FLOW_KEY);
  if (!raw) return;
  const value: unknown = JSON.parse(raw);
  if (
    value &&
    typeof value === "object" &&
    "connectionId" in value &&
    value.connectionId === connectionId
  )
    storage.removeItem(FLOW_KEY);
}

export function clearMcpFlow(storage: Pick<Storage, "removeItem">) {
  storage.removeItem(FLOW_KEY);
}
