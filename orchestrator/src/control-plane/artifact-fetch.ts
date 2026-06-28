/**
 * Collect a session artifact's bytes from the coordinator (ADR 0060 — Slack
 * file forwarding).
 *
 * The coordinator serves artifacts as a server-stream: the FIRST message is
 * `ArtifactMetadata{mediaType,sizeBytes,fileName}`, every subsequent message is
 * a byte chunk (same shape `routes/artifacts.ts` proxies to the browser). That
 * route streams chunks straight through without buffering; here we instead
 * COLLECT the whole artifact into memory so it can be handed to an off-the-shelf
 * SDK (Slack's `files.uploadV2`). Because that means holding the bytes in RAM,
 * a hard cap is enforced both up-front (from the metadata `sizeBytes`) and
 * defensively mid-stream — over the cap throws, and callers fall back to a link.
 */

import { sessions as defaultSessions } from "./client.ts";

/** A fully-collected artifact, ready to hand to an SDK upload call. */
export interface FetchedArtifact {
  bytes: Uint8Array;
  mediaType: string;
  /** The coordinator-supplied filename (may be empty — caller synthesizes one). */
  fileName: string;
}

/** A single `GetArtifact` stream message — the oneof the generated client yields. */
export interface ArtifactStreamMessage {
  msg:
    | { case: "metadata"; value: { mediaType: string; sizeBytes: bigint; fileName: string } }
    | { case: "chunk"; value: Uint8Array }
    | { case: undefined; value?: undefined };
}

/** The `SessionService.getArtifact` subset the collector needs (a structural
 *  seam so a fake satisfies it in tests; the generated client does too). */
export interface ArtifactStreamClient {
  getArtifact(
    req: { sessionId: string; artifactId: string },
    options?: { signal?: AbortSignal },
  ): AsyncIterable<ArtifactStreamMessage>;
}

export interface CollectOptions {
  /** Hard ceiling on the collected size; over this throws (caller falls back). */
  maxBytes: number;
  signal?: AbortSignal;
}

/**
 * Drain `getArtifact` into a single buffer, enforcing `maxBytes` from both the
 * metadata size and the running chunk total. Throws if the stream yields no
 * metadata, or if either check trips. Pure w.r.t. the injected client.
 */
export async function collectArtifactBytes(
  client: ArtifactStreamClient,
  sessionId: string,
  artifactId: string,
  opts: CollectOptions,
): Promise<FetchedArtifact> {
  const stream = client.getArtifact(
    { sessionId, artifactId },
    opts.signal ? { signal: opts.signal } : undefined,
  );

  let mediaType = "";
  let fileName = "";
  let gotMeta = false;
  let total = 0;
  const chunks: Uint8Array[] = [];

  for await (const resp of stream) {
    if (resp.msg.case === "metadata") {
      gotMeta = true;
      mediaType = resp.msg.value.mediaType;
      fileName = resp.msg.value.fileName;
      if (resp.msg.value.sizeBytes > BigInt(opts.maxBytes)) {
        throw new Error(
          `artifact ${artifactId} is ${resp.msg.value.sizeBytes} bytes, over the ${opts.maxBytes}-byte cap`,
        );
      }
    } else if (resp.msg.case === "chunk") {
      total += resp.msg.value.length;
      if (total > opts.maxBytes) {
        throw new Error(`artifact ${artifactId} exceeded the ${opts.maxBytes}-byte cap mid-stream`);
      }
      chunks.push(resp.msg.value);
    }
  }

  if (!gotMeta) throw new Error(`artifact ${artifactId} stream produced no metadata`);

  const bytes = new Uint8Array(total);
  let off = 0;
  for (const c of chunks) {
    bytes.set(c, off);
    off += c.length;
  }
  return { bytes, mediaType, fileName };
}

/** Production fetch: collect an artifact via the singleton coordinator client. */
export function fetchArtifactBytes(
  sessionId: string,
  artifactId: string,
  maxBytes: number,
): Promise<FetchedArtifact> {
  return collectArtifactBytes(
    defaultSessions as unknown as ArtifactStreamClient,
    sessionId,
    artifactId,
    { maxBytes },
  );
}
