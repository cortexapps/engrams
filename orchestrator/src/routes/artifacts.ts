/**
 * Artifact bytes routes (ADR 0051 Task 20 + the cross-session registry).
 *
 * GET /api/v1/sessions/:id/artifacts/:artifactId
 *   The session-scoped route behind the transcript's inline media.
 *   Auth: session guard (owner-or-admin via CASL).
 *
 * GET /api/v1/artifacts/:id[?v=N][&token=...]
 *   The cross-session artifact registry route (stable byte URL behind
 *   /artifacts pages). Auth: EITHER the better-auth cookie/API key (CASL
 *   read on the registry row — owner, org-shared, or admin) OR a
 *   short-lived HMAC `token` capability (crypto/raw-token.ts), which is
 *   how sandboxed (opaque-origin) iframes fetch — they send no SameSite
 *   cookies. `v` selects a version; default = currentVersion.
 *
 * Protocol: both call sessions.getArtifact({sessionId, artifactId}) — a
 * server-stream whose FIRST message is ArtifactMetadata, then chunks.
 * Responses carry the hardened header set (artifact-headers.ts) — that,
 * not the upload gate, is what makes attacker-controlled bytes safe.
 *
 * On upstream NotFound: 404. Client disconnect: AbortSignal propagated.
 * Injectable deps for tests: see makeArtifactsRoute(deps).
 */

import { Hono } from "hono";
import type { Context } from "hono";
import { stream } from "hono/streaming";
import { subject } from "@casl/ability";
import { ConnectError, Code } from "@connectrpc/connect";
import {
  sessions as defaultSessions,
} from "../control-plane/client.ts";
import { makeGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";
import { artifactResponseHeaders } from "./artifact-headers.ts";
import { abilityFor } from "../authz/ability.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { makeArtifactStore, type ArtifactStore } from "../db/artifacts.ts";
import { verifyRawToken } from "../crypto/raw-token.ts";

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

/** Subset of SessionService used by the artifact routes. */
export interface SessionsClient {
  getArtifact(
    req: { sessionId: string; artifactId: string },
    options?: { signal?: AbortSignal },
  ): AsyncIterable<GetArtifactResponse>;
}

/** Injectable deps for the artifact routes. */
export interface ArtifactsDeps {
  sessions?: SessionsClient;
  getSession?: GetSession;
  resolveOwner?: ResolveOwner;
  /** Cross-session registry store (lazy default over the real DB). */
  store?: ArtifactStore;
  /** Raw-token verifier (injectable clock/key in tests). */
  verifyToken?: (artifactId: string, token: string) => boolean;
}

// ---------------------------------------------------------------------------
// Shared streaming core
// ---------------------------------------------------------------------------

/** Open the coordinator stream, consume the metadata frame, and stream
 * the chunk frames with the hardened headers. The registry override
 * (mediaType + stable fileName) wins for presentation; Content-Length
 * always comes from the coordinator metadata — it counts the actual
 * bytes. */
async function streamCoordinatorArtifact(
  c: Context,
  sessionsClient: SessionsClient,
  ref: { sessionId: string; artifactId: string },
  override?: { mediaType: string; fileName: string },
): Promise<Response> {
  const upstream = sessionsClient.getArtifact(ref, { signal: c.req.raw.signal });

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

  // Hardened set (ADR 0026). The registry override wins when present —
  // its fileName is the stable display name.
  const mediaType = override?.mediaType ?? metadata.mediaType;
  const fileName = override?.fileName ?? (metadata.fileName || undefined);
  const headers = artifactResponseHeaders(mediaType, fileName);
  if (metadata.sizeBytes > 0n) {
    headers["Content-Length"] = String(metadata.sizeBytes);
  }

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
  const resolveSession = deps?.getSession ?? getSessionFromHeaders;
  let store = deps?.store;
  const storeOf = (): ArtifactStore => (store ??= makeArtifactStore());
  const verifyToken = deps?.verifyToken ?? verifyRawToken;

  // ---- session-scoped (the transcript's inline media) --------------------
  app.get("/api/v1/sessions/:id/artifacts/:artifactId", async (c) => {
    // Auth + ownership check — throws HTTPException on failure.
    await guardFn(c, "read");
    return streamCoordinatorArtifact(c, sessionsClient, {
      sessionId: c.req.param("id"),
      artifactId: c.req.param("artifactId"),
    });
  });

  // ---- cross-session registry (stable /artifacts byte URL) ---------------
  app.get("/api/v1/artifacts/:id", async (c) => {
    const id = c.req.param("id");
    const token = c.req.query("token");

    // Authenticate: token capability OR cookie/API-key session.
    let authorizedByToken = false;
    if (token !== undefined) {
      authorizedByToken = verifyToken(id, token);
    }
    let actor: { id: string; role: string } | null = null;
    if (!authorizedByToken) {
      const session = await resolveSession(c.req.raw.headers);
      if (!session) return c.json({ error: "unauthenticated" }, 401);
      actor = { id: session.user.id, role: session.user.role ?? "user" };
    }

    const row = await storeOf().get(id);
    if (!row) return c.json({ error: "not found" }, 404);
    if (actor) {
      const ability = abilityFor(actor);
      // Anti-enumeration: an unreadable row is indistinguishable from a
      // missing one.
      if (
        !ability.can(
          "read",
          subject("Artifact", {
            ownerUserId: row.ownerUserId,
            visibility: row.visibility,
          }),
        )
      ) {
        return c.json({ error: "not found" }, 404);
      }
    }

    // Resolve the requested version (default = current).
    const vParam = c.req.query("v");
    const versionNumber = vParam !== undefined ? Number(vParam) : row.currentVersion;
    const version = row.versions.find((v) => v.version === versionNumber);
    if (!Number.isInteger(versionNumber) || !version) {
      return c.json({ error: "not found" }, 404);
    }

    return streamCoordinatorArtifact(
      c,
      sessionsClient,
      { sessionId: version.sessionId, artifactId: version.coordArtifactId },
      { mediaType: version.mediaType, fileName: row.fileName },
    );
  });

  return app;
}

export default makeArtifactsRoute();
