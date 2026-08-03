/**
 * Short-lived HMAC capability tokens for the artifact byte route.
 *
 * A sandboxed (opaque-origin) iframe does not send SameSite cookies, so
 * the web embeds artifact HTML with a tokened URL instead:
 *   /api/v1/artifacts/<id>?token=<expUnix>.<b64url(hmac)>
 * The token is minted into List/Get responses AFTER the caller proved
 * read access — it is a bearer capability for that one artifact id, TTL
 * ~10 minutes (consumed by the initial iframe navigation only).
 *
 * Stateless and multi-pod safe: the key is HKDF-derived from the
 * deployment KEK (ENGRAM_KEK_MASTER_KEY — already required, shared by
 * every orchestrator pod), info-bound to this use so the KEK never
 * signs anything directly.
 */

import { createHmac, hkdfSync } from "node:crypto";

import { config } from "../config.ts";
import { constantTimeEquals } from "./constant-time.ts";

/** Token lifetime. Long enough for a page to mount its iframes; short
 * enough that a leaked URL goes stale quickly. */
export const RAW_TOKEN_TTL_MS = 10 * 60 * 1000;

const HKDF_INFO = "artifact-raw-token";
const SCOPE_PREFIX = "artifact-raw:";

const keyCache = new Map<string, Buffer>();

function signingKey(kekBase64: string): Buffer {
  const cached = keyCache.get(kekBase64);
  if (cached) return cached;
  const kek = Buffer.from(kekBase64, "base64");
  const key = Buffer.from(
    hkdfSync("sha256", kek, Buffer.alloc(0), Buffer.from(HKDF_INFO), 32),
  );
  keyCache.set(kekBase64, key);
  return key;
}

function signature(artifactId: string, expUnixMs: number, kekBase64: string): string {
  return createHmac("sha256", signingKey(kekBase64))
    .update(`${SCOPE_PREFIX}${artifactId}:${expUnixMs}`)
    .digest("base64url");
}

/** Mint a token for one artifact id. `now` is injectable for tests. */
export function mintRawToken(
  artifactId: string,
  now: Date = new Date(),
  kekBase64: string = config.kekMasterKey,
): string {
  const exp = now.getTime() + RAW_TOKEN_TTL_MS;
  return `${exp}.${signature(artifactId, exp, kekBase64)}`;
}

/** Verify a token for one artifact id: shape, expiry, then a
 * constant-time signature compare. */
export function verifyRawToken(
  artifactId: string,
  token: string,
  now: Date = new Date(),
  kekBase64: string = config.kekMasterKey,
): boolean {
  const dot = token.indexOf(".");
  if (dot <= 0) return false;
  const exp = Number(token.slice(0, dot));
  if (!Number.isFinite(exp) || exp <= now.getTime()) return false;
  return constantTimeEquals(token.slice(dot + 1), signature(artifactId, exp, kekBase64));
}

/** The byte-route path (relative to the app origin) with the token
 * attached — what ArtifactRecord.raw_url carries. */
export function rawArtifactPath(artifactId: string, token: string): string {
  return `/api/v1/artifacts/${encodeURIComponent(artifactId)}?token=${encodeURIComponent(token)}`;
}
