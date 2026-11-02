import { afterEach, expect, it, vi } from "vitest";

afterEach(() => {
  vi.resetModules();
  vi.doUnmock("./mcp-callback");
  vi.doUnmock("./app-entry");
  vi.doUnmock("./mcp-callback-bootstrap");
  document.body.innerHTML = "";
  window.history.replaceState(null, "", "/");
});

it("scrubs the callback before loading callback code or the app router", async () => {
  window.history.replaceState(
    null,
    "",
    "/integrations/mcp/callback?code=secret&state=opaque#fragment",
  );
  const received: { code: string; state: string; issuer?: string }[] = [];
  const complete = vi.fn((value) => {
    received.push(value);
  });
  const app = vi.fn();
  vi.doMock("./mcp-callback", () => {
    expect(window.location.search).toBe("");
    expect(window.location.hash).toBe("");
    return { completeCallback: complete };
  });
  vi.doMock("./app-entry", () => {
    app();
    return {};
  });
  await import("./main");
  await vi.waitFor(() => expect(complete).toHaveBeenCalledOnce());
  expect(app).not.toHaveBeenCalled();
  await vi.waitFor(() => expect(received[0]).toEqual({ code: "", state: "", issuer: undefined }));
});

it("clears captured values when loading the callback module fails", async () => {
  document.body.innerHTML = '<div id="root"></div>';
  window.history.replaceState(
    null,
    "",
    "/integrations/mcp/callback?code=secret&state=opaque&iss=issuer",
  );
  let captured: { code: string; state: string; issuer?: string } | null | undefined;
  vi.doMock("./mcp-callback-bootstrap", async (importOriginal) => {
    const actual = await importOriginal<typeof import("./mcp-callback-bootstrap")>();
    return {
      ...actual,
      captureMcpCallback: (...args: Parameters<typeof actual.captureMcpCallback>) => {
        captured = actual.captureMcpCallback(...args);
        return captured;
      },
    };
  });
  vi.doMock("./mcp-callback", () => {
    throw new Error("module unavailable");
  });
  await import("./main");
  await vi.waitFor(() => expect(document.body.textContent).toContain("restart or recover"));
  expect(captured).toEqual({ code: "", state: "", issuer: undefined });
});

it("clears the transferred callback when completion rejects", async () => {
  document.body.innerHTML = '<div id="root"></div>';
  window.history.replaceState(
    null,
    "",
    "/integrations/mcp/callback?code=secret&state=opaque&iss=issuer",
  );
  let received: { code: string; state: string; issuer?: string } | null = null;
  vi.doMock("./mcp-callback", () => ({
    completeCallback: async (value: { code: string; state: string; issuer?: string }) => {
      received = value;
      throw new Error("completion failed");
    },
  }));
  await import("./main");
  await vi.waitFor(() => expect(document.body.textContent).toContain("restart or recover"));
  expect(received).toEqual({ code: "", state: "", issuer: undefined });
});
