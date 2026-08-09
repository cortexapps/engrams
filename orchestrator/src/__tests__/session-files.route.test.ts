import { describe, expect, test } from "bun:test";

import {
  makeSessionFilesRoute,
  type SessionFileClient,
} from "../routes/session-files.ts";

const sessionId = "019fe2ff-0464-75f3-bb20-a8c1844579b9";
const uploadId = "119fe2ff-0464-75f3-bb20-a8c1844579b9";
const path = `/tmp/uploads/${uploadId}/notes.txt`;
const bytes = new TextEncoder().encode("notes");
const digest = "0".repeat(64);

function app(client: SessionFileClient, authenticated = true) {
  return makeSessionFilesRoute({
    sessions: client,
    getSession: async () =>
      authenticated ? { user: { id: "user-1", role: "user" } } : null,
    resolveOwner: async () => "user-1",
  });
}

describe("session file routes", () => {
  test("streams upload metadata first and then exact chunks", async () => {
    const frames: Array<{ frame: { case: string; value: unknown } }> = [];
    const client: SessionFileClient = {
      async writeFile(input) {
        for await (const frame of input) frames.push(frame);
        return { path, sizeBytes: BigInt(bytes.length), sha256: digest };
      },
      async *readFile() {},
    };
    const response = await app(client).request(
      `/api/v1/sessions/${sessionId}/files?path=${encodeURIComponent(path)}`,
      {
        method: "POST",
        body: bytes,
        headers: {
          "x-upload-size": String(bytes.length),
          "x-upload-sha256": digest,
        },
      },
    );
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      path,
      size_bytes: bytes.length,
      sha256: digest,
    });
    expect(frames[0]).toEqual({
      frame: {
        case: "metadata",
        value: {
          sessionId,
          path,
          sizeBytes: BigInt(bytes.length),
          sha256: digest,
        },
      },
    });
    expect(
      frames
        .slice(1)
        .flatMap((frame) => Array.from(frame.frame.value as Uint8Array)),
    ).toEqual(Array.from(bytes));
  });

  test("rejects unauthenticated and oversized uploads before the RPC", async () => {
    let called = false;
    const client: SessionFileClient = {
      async writeFile() {
        called = true;
        throw new Error("not reached");
      },
      async *readFile() {},
    };
    const unauthenticated = await app(client, false).request(
      `/api/v1/sessions/${sessionId}/files`,
      { method: "POST" },
    );
    expect(unauthenticated.status).toBe(401);
    const oversized = await app(client).request(
      `/api/v1/sessions/${sessionId}/files?path=${encodeURIComponent(path)}`,
      {
        method: "POST",
        headers: { "x-upload-size": String(512 * 1024 * 1024 + 1) },
      },
    );
    expect(oversized.status).toBe(413);
    expect(called).toBeFalse();
  });

  test("streams authenticated downloads with verified metadata headers", async () => {
    const client: SessionFileClient = {
      async writeFile() {
        throw new Error("not reached");
      },
      async *readFile(input) {
        expect(input).toEqual({ sessionId, path });
        yield {
          frame: {
            case: "metadata",
            value: {
              path,
              sizeBytes: BigInt(bytes.length),
              sha256: digest,
              fileName: "notes.txt",
            },
          },
        };
        yield { frame: { case: "chunk", value: bytes } };
      },
    };
    const response = await app(client).request(
      `/api/v1/sessions/${sessionId}/files?path=${encodeURIComponent(path)}`,
    );
    expect(response.status).toBe(200);
    expect(response.headers.get("content-length")).toBe(String(bytes.length));
    expect(response.headers.get("x-content-sha256")).toBe(digest);
    expect(new Uint8Array(await response.arrayBuffer())).toEqual(bytes);
  });
});
