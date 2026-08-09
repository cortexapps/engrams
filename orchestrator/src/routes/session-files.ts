/** Authenticated browser upload and download routes (ADR 0113). */

import { Code, ConnectError } from "@connectrpc/connect";
import { Hono } from "hono";

import { sessions as defaultSessions } from "../control-plane/client.ts";
import { makeGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";

const MAX_FILE_BYTES = 512 * 1024 * 1024;

export interface SessionFileClient {
  writeFile(
    input: AsyncIterable<{
      frame:
        | {
            case: "metadata";
            value: {
              sessionId: string;
              path: string;
              sizeBytes: bigint;
              sha256: string;
              mode?: number;
            };
          }
        | { case: "chunk"; value: Uint8Array };
    }>,
  ): Promise<{ path: string; sizeBytes: bigint; sha256: string }>;
  readFile(input: { sessionId: string; path: string }): AsyncIterable<{
    frame:
      | {
          case: "metadata";
          value: {
            path: string;
            sizeBytes: bigint;
            sha256: string;
            fileName: string;
          };
        }
      | { case: "chunk"; value: Uint8Array }
      | { case: undefined; value?: undefined };
  }>;
}

export interface SessionFileRouteDeps {
  sessions?: SessionFileClient;
  getSession?: GetSession;
  resolveOwner?: ResolveOwner;
}

function errorResponse(error: unknown): Response {
  if (error instanceof ConnectError) {
    const status =
      error.code === Code.ResourceExhausted
        ? 413
        : error.code === Code.NotFound
          ? 404
          : error.code === Code.PermissionDenied
            ? 403
            : error.code === Code.Unauthenticated
              ? 401
              : error.code === Code.AlreadyExists || error.code === Code.FailedPrecondition
                ? 409
                : 400;
    return Response.json({ error: error.rawMessage }, { status });
  }
  return Response.json(
    { error: error instanceof Error ? error.message : String(error) },
    { status: 500 },
  );
}

export function makeSessionFilesRoute(deps: SessionFileRouteDeps = {}): Hono {
  const app = new Hono();
  const client = deps.sessions ?? (defaultSessions as unknown as SessionFileClient);
  const guard = makeGuard(deps.getSession, deps.resolveOwner);

  app.post("/api/v1/sessions/:id/files", async (c) => {
    await guard(c, "shell");
    const path = c.req.query("path") ?? "";
    if (!path) return c.json({ error: "path is required" }, 400);
    const sha256 = c.req.header("x-upload-sha256") ?? "";
    const rawSize = c.req.header("x-upload-size") ?? c.req.header("content-length") ?? "";
    if (!/^\d+$/.test(rawSize)) {
      return c.json({ error: "x-upload-size must be a non-negative integer" }, 400);
    }
    const sizeBytes = BigInt(rawSize);
    if (sizeBytes > BigInt(MAX_FILE_BYTES)) {
      return c.json({ error: `file exceeds ${MAX_FILE_BYTES} bytes` }, 413);
    }
    const body = c.req.raw.body;
    if (body == null && sizeBytes !== 0n) {
      return c.json({ error: "request body is required" }, 400);
    }
    async function* frames() {
      yield {
        frame: {
          case: "metadata" as const,
          value: {
            sessionId: c.req.param("id"),
            path,
            sizeBytes,
            sha256,
          },
        },
      };
      if (body == null) return;
      const reader = body.getReader();
      try {
        while (true) {
          const result = await reader.read();
          if (result.done) return;
          yield {
            frame: { case: "chunk" as const, value: result.value },
          };
        }
      } finally {
        reader.releaseLock();
      }
    }
    try {
      const result = await client.writeFile(frames());
      return c.json({
        path: result.path,
        size_bytes: Number(result.sizeBytes),
        sha256: result.sha256,
      });
    } catch (error) {
      return errorResponse(error);
    }
  });

  app.get("/api/v1/sessions/:id/files", async (c) => {
    await guard(c, "read");
    const path = c.req.query("path");
    if (!path) return c.json({ error: "path is required" }, 400);
    try {
      const iterator = client
        .readFile({ sessionId: c.req.param("id"), path })
        [Symbol.asyncIterator]();
      const first = await iterator.next();
      if (first.done || first.value.frame.case !== "metadata") {
        return c.json({ error: "file stream did not start with metadata" }, 502);
      }
      const metadata = first.value.frame.value;
      const stream = new ReadableStream<Uint8Array>({
        async pull(controller) {
          try {
            const next = await iterator.next();
            if (next.done) {
              controller.close();
            } else if (next.value.frame.case === "chunk") {
              controller.enqueue(next.value.frame.value);
            } else {
              controller.error(new Error("file stream repeated metadata"));
            }
          } catch (error) {
            controller.error(error);
          }
        },
        async cancel() {
          await iterator.return?.();
        },
      });
      return new Response(stream, {
        headers: {
          "content-type": "application/octet-stream",
          "content-length": metadata.sizeBytes.toString(),
          "content-disposition": `attachment; filename*=UTF-8''${encodeURIComponent(metadata.fileName)}`,
          "x-content-sha256": metadata.sha256,
        },
      });
    } catch (error) {
      return errorResponse(error);
    }
  });

  return app;
}

export default makeSessionFilesRoute();
