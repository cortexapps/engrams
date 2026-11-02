import { createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-web";
import { IntegrationService } from "./gen/engram/app/v1/integration_pb";
import { API_CREDENTIALS, RPC_BASE } from "./lib/base";
import { consumeMcpFlow, MCP_CALLBACK_PATH } from "./mcp-callback-bootstrap";

export async function completeCallback(
  callback: { code: string; state: string; issuer?: string } | null,
) {
  const root = document.getElementById("root");
  if (!root) return;
  root.textContent = "Completing authorization…";
  const redirectUri = `${window.location.origin}${MCP_CALLBACK_PATH}`;
  let flow: ReturnType<typeof consumeMcpFlow> = null;
  try {
    flow = consumeMcpFlow(window.sessionStorage, redirectUri);
    if (!callback || !flow) throw new Error("Missing authorization context");
    const transport = createConnectTransport({
      baseUrl: RPC_BASE,
      fetch: (input, init) =>
        fetch(input, { ...init, credentials: API_CREDENTIALS, cache: "no-store" }),
    });
    await createClient(IntegrationService, transport).completeMcpAuthorization({
      connectionId: flow.connectionId,
      flowId: flow.flowId,
      expectedRevision: flow.configRevision,
      expectedGeneration: flow.authGeneration,
      redirectUri: flow.redirectUri,
      code: callback.code,
      state: callback.state,
      issuer: callback.issuer,
    });
    root.textContent = "Authorization complete. Return to integrations to review the connection.";
  } catch {
    root.textContent =
      "Authorization could not be completed. Return to integrations to restart or recover the connection.";
  } finally {
    callback = null;
    flow = null;
  }
  const link = document.createElement("a");
  link.href = "/settings/integrations";
  link.textContent = "Return to integrations";
  root.append(document.createElement("br"), link);
}
