import { describe, expect, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";

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
    expect(response.headers.has("x-engram-rpc-code")).toBe(false);
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
    expect(response.headers.has("x-engram-rpc-code")).toBe(false);
    expect(response.headers.get("content-length")).toBe(String(bytes.length));
    expect(response.headers.get("x-content-sha256")).toBe(digest);
    expect(new Uint8Array(await response.arrayBuffer())).toEqual(bytes);
  });

  test("keeps file RPC status and body while exposing only its numeric code in the diagnostic header", async () => {
    const cases: Array<[Code, number]> = [
      [Code.Canceled, 400],
      [Code.Unknown, 400],
      [Code.InvalidArgument, 400],
      [Code.DeadlineExceeded, 400],
      [Code.NotFound, 404],
      [Code.AlreadyExists, 409],
      [Code.PermissionDenied, 403],
      [Code.ResourceExhausted, 413],
      [Code.FailedPrecondition, 409],
      [Code.Aborted, 400],
      [Code.OutOfRange, 400],
      [Code.Unimplemented, 400],
      [Code.Internal, 400],
      [Code.Unavailable, 400],
      [Code.DataLoss, 400],
      [Code.Unauthenticated, 401],
    ];
    for (const [code, status] of cases) {
      const error = new ConnectError("file RPC failed", code, {
        "x-private": "private-error-metadata",
      });
      const client: SessionFileClient = {
        async writeFile() {
          throw error;
        },
        async *readFile() {
          throw error;
        },
      };
      for (const method of ["GET", "POST"]) {
        const response = await app(client).request(
          `/api/v1/sessions/${sessionId}/files?path=${encodeURIComponent(path)}`,
          { method, ...(method === "POST" ? { headers: { "x-upload-size": "0" } } : {}) },
        );
        expect(response.status).toBe(status);
        expect(response.headers.get("x-engram-rpc-code")).toBe(String(code));
        expect(response.headers.has("x-private")).toBe(false);
        expect(await response.json()).toEqual({ error: "file RPC failed" });
      }
    }
  });

  test("omits the diagnostic header for invalid RPC error codes", async () => {
    for (const code of [0, 17, -1, 1.5, NaN, Infinity, -Infinity]) {
      const error = new ConnectError("file RPC failed", code);
      const client: SessionFileClient = {
        async writeFile() {
          throw error;
        },
        async *readFile() {
          throw error;
        },
      };
      for (const method of ["GET", "POST"]) {
        const response = await app(client).request(
          `/api/v1/sessions/${sessionId}/files?path=${encodeURIComponent(path)}`,
          { method, ...(method === "POST" ? { headers: { "x-upload-size": "0" } } : {}) },
        );
        expect(response.status).toBe(400);
        expect(response.headers.has("x-engram-rpc-code")).toBe(false);
        expect(await response.json()).toEqual({ error: "file RPC failed" });
      }
    }
  });

  test("does not give non-RPC file errors an RPC diagnostic code", async () => {
    const client: SessionFileClient = {
      async writeFile() {
        throw new Error("file failed");
      },
      async *readFile() {
        throw new Error("file failed");
      },
    };
    for (const method of ["GET", "POST"]) {
      const response = await app(client).request(
        `/api/v1/sessions/${sessionId}/files?path=${encodeURIComponent(path)}`,
        { method, ...(method === "POST" ? { headers: { "x-upload-size": "0" } } : {}) },
      );
      expect(response.status).toBe(500);
      expect(response.headers.has("x-engram-rpc-code")).toBe(false);
      expect(await response.json()).toEqual({ error: "file failed" });
    }
  });
});
