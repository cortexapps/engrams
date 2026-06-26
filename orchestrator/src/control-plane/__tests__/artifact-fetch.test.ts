/**
 * collectArtifactBytes (ADR 0060 — Slack file forwarding).
 *
 * The collector drains the coordinator's `GetArtifact` server-stream into one
 * buffer and enforces a hard size cap. Exercised through a fake stream client so
 * no engine or network is needed.
 */

import { expect, test, describe } from "bun:test";
import {
  collectArtifactBytes,
  type ArtifactStreamClient,
  type ArtifactStreamMessage,
} from "../artifact-fetch.ts";

/** Build a fake `getArtifact` client yielding `metadata` then the given chunks. */
function fakeClient(
  meta: { mediaType: string; sizeBytes: bigint; fileName: string } | null,
  chunks: Uint8Array[],
  onCall?: (req: { sessionId: string; artifactId: string }) => void,
): ArtifactStreamClient {
  return {
    getArtifact(req) {
      onCall?.(req);
      async function* gen(): AsyncGenerator<ArtifactStreamMessage> {
        if (meta) yield { msg: { case: "metadata", value: meta } };
        for (const c of chunks) yield { msg: { case: "chunk", value: c } };
      }
      return gen();
    },
  };
}

describe("collectArtifactBytes()", () => {
  test("concatenates chunks and returns metadata", async () => {
    const client = fakeClient({ mediaType: "image/png", sizeBytes: 5n, fileName: "shot.png" }, [
      new Uint8Array([1, 2, 3]),
      new Uint8Array([4, 5]),
    ]);
    const out = await collectArtifactBytes(client, "s1", "a1", { maxBytes: 1024 });
    expect(Array.from(out.bytes)).toEqual([1, 2, 3, 4, 5]);
    expect(out.mediaType).toBe("image/png");
    expect(out.fileName).toBe("shot.png");
  });

  test("passes session + artifact ids through to the client", async () => {
    let seen: { sessionId: string; artifactId: string } | undefined;
    const client = fakeClient(
      { mediaType: "image/png", sizeBytes: 1n, fileName: "" },
      [new Uint8Array([7])],
      (req) => (seen = req),
    );
    await collectArtifactBytes(client, "sess-9", "art-9", { maxBytes: 1024 });
    expect(seen).toEqual({ sessionId: "sess-9", artifactId: "art-9" });
  });

  test("rejects when the metadata size exceeds the cap (before buffering)", async () => {
    const client = fakeClient({ mediaType: "video/mp4", sizeBytes: 2048n, fileName: "v.mp4" }, [
      new Uint8Array([1]),
    ]);
    await expect(collectArtifactBytes(client, "s1", "big", { maxBytes: 1024 })).rejects.toThrow(
      /over the 1024-byte cap/,
    );
  });

  test("rejects when chunks exceed the cap mid-stream (size lied/absent)", async () => {
    // metadata claims 1 byte but the stream actually sends 1500.
    const client = fakeClient({ mediaType: "image/png", sizeBytes: 1n, fileName: "x.png" }, [
      new Uint8Array(1000),
      new Uint8Array(500),
    ]);
    await expect(collectArtifactBytes(client, "s1", "a1", { maxBytes: 1024 })).rejects.toThrow(
      /exceeded the 1024-byte cap mid-stream/,
    );
  });

  test("rejects a stream that yields no metadata", async () => {
    const client = fakeClient(null, [new Uint8Array([1, 2])]);
    await expect(collectArtifactBytes(client, "s1", "a1", { maxBytes: 1024 })).rejects.toThrow(
      /no metadata/,
    );
  });
});
