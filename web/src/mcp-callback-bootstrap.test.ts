import { describe, expect, it, vi } from "vitest";
import {
  captureMcpCallback,
  consumeMcpFlow,
  saveMcpFlow,
  MCP_CALLBACK_PATH,
} from "./mcp-callback-bootstrap";

const flow = {
  connectionId: "c1",
  flowId: "f1",
  configRevision: 2,
  authGeneration: 3,
  redirectUri: `https://app.example${MCP_CALLBACK_PATH}`,
  expiresAt: "2099-01-01T00:00:00Z",
};

describe("MCP callback bootstrap", () => {
  it("scrubs the whole URL synchronously before returning callback values", () => {
    const replaceState = vi.fn();
    const result = captureMcpCallback(
      new URL(`${flow.redirectUri}?code=secret&state=opaque&iss=issuer#fragment`),
      { replaceState },
    );
    expect(replaceState).toHaveBeenCalledWith(null, "", MCP_CALLBACK_PATH);
    expect(result).toEqual({ code: "secret", state: "opaque", issuer: "issuer" });
  });

  it("scrubs and rejects ambiguous parameters", () => {
    const replaceState = vi.fn();
    expect(
      captureMcpCallback(new URL(`${flow.redirectUri}?code=a&code=b&state=s`), { replaceState }),
    ).toBeNull();
    expect(replaceState).toHaveBeenCalledOnce();
  });

  it("stores only correlation metadata, rejects an active flow and consumes once", () => {
    const storage = new Map<string, string>();
    const store = {
      getItem: (key: string) => storage.get(key) ?? null,
      setItem: (key: string, value: string) => {
        storage.set(key, value);
      },
      removeItem: (key: string) => {
        storage.delete(key);
      },
    };
    saveMcpFlow(store, flow);
    expect(() => saveMcpFlow(store, flow)).toThrow();
    expect(consumeMcpFlow(store, flow.redirectUri, 0)).toEqual(flow);
    expect(consumeMcpFlow(store, flow.redirectUri, 0)).toBeNull();
  });

  it("rejects expired and wrong redirect records", () => {
    for (const [record, redirect, now] of [
      [flow, flow.redirectUri, Date.parse("2100-01-01T00:00:00Z")],
      [flow, "https://other.example/integrations/mcp/callback", 0],
    ] as const) {
      const store = { getItem: () => JSON.stringify(record), removeItem: vi.fn() };
      expect(consumeMcpFlow(store, redirect, now)).toBeNull();
      expect(store.removeItem).toHaveBeenCalledOnce();
    }
  });

  it("fails closed when storage access fails", () => {
    const store = {
      getItem: () => {
        throw new Error("blocked");
      },
      removeItem: vi.fn(),
    };
    expect(consumeMcpFlow(store, flow.redirectUri, 0)).toBeNull();
  });

  it("rejects extra fields after removal", () => {
    const store = {
      getItem: () => JSON.stringify({ ...flow, secret: "bad" }),
      removeItem: vi.fn(),
    };
    expect(consumeMcpFlow(store, flow.redirectUri, 0)).toBeNull();
    expect(store.removeItem).toHaveBeenCalledOnce();
  });
});
