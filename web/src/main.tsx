import { captureMcpCallback, clearMcpCallback } from "./mcp-callback-bootstrap";

async function bootstrap() {
  let callback = captureMcpCallback(new URL(window.location.href), window.history);
  if (callback === undefined) {
    void import("./app-entry");
    return;
  }
  try {
    const { completeCallback } = await import("./mcp-callback");
    await completeCallback(callback);
  } catch {
    const root = document.getElementById("root");
    if (root)
      root.textContent =
        "Authorization could not be completed. Return to integrations to restart or recover the connection.";
  } finally {
    clearMcpCallback(callback);
    callback = null;
  }
}

void bootstrap();
