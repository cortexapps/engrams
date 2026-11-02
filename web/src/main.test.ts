import { afterEach, expect, it, vi } from "vitest";

afterEach(() => {
  vi.resetModules();
  vi.doUnmock("./mcp-callback");
  vi.doUnmock("./app-entry");
  window.history.replaceState(null, "", "/");
});

it("scrubs the callback before loading callback code or the app router", async () => {
  window.history.replaceState(
    null,
    "",
    "/integrations/mcp/callback?code=secret&state=opaque#fragment",
  );
  const complete = vi.fn();
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
  expect(complete).toHaveBeenCalledWith({ code: "secret", state: "opaque", issuer: undefined });
});
