/**
 * Artifact bytes route (ADR 0051 Task 20).
 *
 * GET /api/v1/sessions/:id/artifacts/:artifactId
 *
 * Mirrors the coordinator's artifact-serve endpoint (coordinator serves at
 * /sessions/:id/artifacts/:artifact_id; API_BASE = /api/v1 in the web, so the
 * full path is /api/v1/sessions/:id/artifacts/:artifact_id). Path matches
 * web/src/components/session-thread/SystemMessage.tsx:
 *   const src = `${API_BASE}/sessions/${marker.sessionId}/artifacts/${marker.artifactId}`;
 *
 * Protocol: calls sessions.getArtifact({sessionId, artifactId}) which is a
 * server-stream. FIRST message is ArtifactMetadata{mediaType,sizeBytes,fileName};
 * subsequent messages are byte chunks.
 *
 * The response streams: Content-Type from metadata, Content-Length from
 * sizeBytes (when > 0), body = concatenated chunks.
 *
 * On upstream NotFound: responds 404.
 * Client disconnect: AbortSignal propagated to upstream gRPC stream.
 *
 * Injectable deps for tests: see makeArtifactsRoute(deps).
 */

import { Hono } from "hono";
import { stream } from "hono/streaming";
import { ConnectError, Code } from "@connectrpc/connect";
import {
  sessions as defaultSessions,
} from "../control-plane/client.ts";
import { makeGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";
import { artifactResponseHeaders } from "./artifact-headers.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface ArtifactMetadata {
  mediaType: string;
  sizeBytes: bigint;
  fileName: string;
}

export interface GetArtifactResponse {
  msg:
    | { case: "metadata"; value: ArtifactMetadata }
    | { case: "chunk"; value: Uint8Array }
    | { case: undefined; value?: undefined };
}

/** Subset of SessionService used by the artifact route. */
export interface SessionsClient {
  getArtifact(
    req: { sessionId: string; artifactId: string },
    options?: { signal?: AbortSignal },
  ): AsyncIterable<GetArtifactResponse>;
}

/** Injectable deps for the artifacts route. */
export interface ArtifactsDeps {
  sessions?: SessionsClient;
  getSession?: GetSession;
  resolveOwner?: ResolveOwner;
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

export function makeArtifactsRoute(deps?: ArtifactsDeps): Hono {
  const app = new Hono();
  const sessionsClient: SessionsClient =
    (deps?.sessions as SessionsClient | undefined) ??
    (defaultSessions as unknown as SessionsClient);
  const guardFn = makeGuard(deps?.getSession, deps?.resolveOwner);

  app.get("/api/v1/sessions/:id/artifacts/:artifactId", async (c) => {
    // 1. Auth + ownership check — throws HTTPException on failure.
    await guardFn(c, "read");

    const sessionId = c.req.param("id");
    const artifactId = c.req.param("artifactId");

    // 2. Open upstream server-stream.
    const upstream = sessionsClient.getArtifact(
      { sessionId, artifactId },
      { signal: c.req.raw.signal },
    );

    // 3. Consume the first message (metadata); on NotFound return 404.
    let metadata: ArtifactMetadata | null = null;
    let restIter!: AsyncIterator<GetArtifactResponse>;

    try {
      const iter = upstream[Symbol.asyncIterator]();
      restIter = iter;

      const first = await iter.next();
      if (first.done) {
        return c.json({ error: "empty artifact stream" }, 500);
      }
      const firstMsg = first.value;
      if (firstMsg.msg.case !== "metadata") {
        return c.json({ error: "expected metadata as first message" }, 500);
      }
      metadata = firstMsg.msg.value;
    } catch (err) {
      // Upstream NotFound → 404.
      if (err instanceof ConnectError && err.code === Code.NotFound) {
        return c.json({ error: "artifact not found" }, 404);
      }
      throw err;
    }

    // 4. Build response headers from metadata (hardened set, ADR 0026).
    const headers = artifactResponseHeaders(
      metadata.mediaType,
      metadata.fileName || undefined,
    );
    if (metadata.sizeBytes > 0n) {
      headers["Content-Length"] = String(metadata.sizeBytes);
    }

    // 5. Stream the remaining chunk messages to the browser.
    return stream(c, async (s) => {
      // Write headers before the first byte.
      for (const [k, v] of Object.entries(headers)) {
        c.header(k, v);
      }

      try {
        while (true) {
          const next = await restIter.next();
          if (next.done) break;
          const msg = next.value;
          if (msg.msg.case === "chunk") {
            await s.write(msg.msg.value);
          }
        }
      } catch (err) {
        // Client disconnect is expected — suppress. connect-es surfaces
        // aborts as ConnectError(Canceled), plain fetch as AbortError.
        if (err instanceof ConnectError && err.code === Code.Canceled) return;
        if (err instanceof Error && err.name === "AbortError") return;
        throw err;
      }
    });
  });

  return app;
}

export default makeArtifactsRoute();
