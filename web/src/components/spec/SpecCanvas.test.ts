import { afterEach, describe, expect, test } from "vitest";
import * as Y from "yjs";

import { createSpecProvider } from "./SpecCanvas";

const documents: Y.Doc[] = [];

afterEach(() => {
  for (const document of documents.splice(0)) document.destroy();
});

describe("the spec WebSocket provider", () => {
  test("uses the server sync route and one client id query parameter", () => {
    const document = new Y.Doc();
    documents.push(document);
    const provider = createSpecProvider("spec/one", document, {
      connect: false,
      location: { protocol: "https:", host: "engrams.test" },
      WebSocketPolyfill: window.WebSocket,
    });

    expect(provider.roomname).toBe("sync");
    expect(provider.url).toBe(
      `wss://engrams.test/api/v1/specs/spec%2Fone/sync?clientId=${document.clientID}`,
    );
    expect(new URL(provider.url).searchParams.getAll("clientId")).toEqual([
      String(document.clientID),
    ]);

    provider.destroy();
  });
});
