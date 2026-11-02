import { afterEach, describe, expect, it, vi } from "vitest";
import { MCP_CALLBACK_PATH, saveMcpFlow } from "./mcp-callback-bootstrap";

const redirectUri = `${window.location.origin}${MCP_CALLBACK_PATH}`;
const flow = {
  connectionId: "c1",
  flowId: "f1",
  configRevision: 2,
  authGeneration: 3,
  redirectUri,
  expiresAt: "2099-01-01T00:00:00Z",
};

afterEach(() => {
  vi.restoreAllMocks();
  sessionStorage.clear();
  document.body.innerHTML = "";
  delete window.__ENGRAM_API_ORIGIN__;
  vi.resetModules();
});

describe("MCP callback", () => {
  it("posts once with the configured transport and consumes metadata", async () => {
    document.body.innerHTML = '<div id="root"></div>';
    saveMcpFlow(sessionStorage, flow);
    const fetchSpy = vi
      .spyOn(window, "fetch")
      .mockResolvedValue(new Response("{}", { headers: { "content-type": "application/json" } }));
    const { completeCallback } = await import("./mcp-callback");
    await completeCallback({ code: "code-secret", state: "state-secret" });
    expect(fetchSpy).toHaveBeenCalledOnce();
    const [url, options] = fetchSpy.mock.calls[0];
    expect(String(url)).toContain("/rpc/engram.app.v1.IntegrationService/CompleteMcpAuthorization");
    expect(options?.credentials).toBe("same-origin");
    expect(options?.cache).toBe("no-store");
    expect(new TextDecoder().decode(options?.body as Uint8Array)).toContain("code-secret");
    expect(sessionStorage.length).toBe(0);
    await completeCallback({ code: "code-secret", state: "state-secret" });
    expect(fetchSpy).toHaveBeenCalledOnce();
    expect(document.body.textContent).toContain("restart or recover");
  });

  it.each([400, 401, 403])(
    "uses split-host credentials and does not retry after a %s rejection",
    async (status) => {
      document.body.innerHTML = '<div id="root"></div>';
      window.__ENGRAM_API_ORIGIN__ = "https://api.example.test";
      saveMcpFlow(sessionStorage, flow);
      const fetchSpy = vi
        .spyOn(window, "fetch")
        .mockResolvedValue(
          new Response("{}", { status, headers: { "content-type": "application/json" } }),
        );
      vi.resetModules();
      const { completeCallback } = await import("./mcp-callback");
      await completeCallback({
        code: "code-secret",
        state: "state-secret",
        issuer: "https://issuer.example.test",
      });
      expect(fetchSpy).toHaveBeenCalledOnce();
      expect(String(fetchSpy.mock.calls[0][0])).toMatch(/^https:\/\/api\.example\.test\/rpc\//);
      expect(fetchSpy.mock.calls[0][1]?.credentials).toBe("include");
      expect(sessionStorage.length).toBe(0);
      expect(document.body.textContent).toContain("restart or recover");
    },
  );
});
