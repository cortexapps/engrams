import { describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";

import { stageFiles, type FileStagingClient, type WriteFileFrame } from "../stage-files.ts";

function client(options: { failPaths?: string[] } = {}) {
  const frames: WriteFileFrame[] = [];
  const stagingClient: FileStagingClient = {
    async writeFile(input) {
      const collected: WriteFileFrame[] = [];
      for await (const { frame } of input) collected.push(frame);
      const metadata = collected[0];
      if (!metadata || metadata.case !== "metadata") throw new Error("no metadata frame");
      if (options.failPaths?.includes(metadata.value.path)) {
        throw new Error(`agentd rejected ${metadata.value.path}`);
      }
      frames.push(...collected);
      return {
        path: metadata.value.path,
        sizeBytes: metadata.value.sizeBytes,
        sha256: metadata.value.sha256,
      };
    },
  };
  return { stagingClient, frames };
}

const text = (s: string) => new TextEncoder().encode(s);

describe("stageFiles", () => {
  test("streams a metadata frame then one chunk, content-addressed", async () => {
    const { stagingClient, frames } = client();
    const content = text("hello world\n");
    const results = await stageFiles(stagingClient, "session-1", [
      { path: "/workspace/.review/finder.md", content, mode: 0o644 },
    ]);

    expect(results).toEqual([{ path: "/workspace/.review/finder.md", ok: true }]);
    expect(frames).toHaveLength(2);
    const metadata = frames[0]!;
    if (metadata.case !== "metadata") throw new Error("expected metadata first");
    expect(metadata.value).toEqual({
      sessionId: "session-1",
      path: "/workspace/.review/finder.md",
      sizeBytes: BigInt(content.byteLength),
      sha256: createHash("sha256").update(content).digest("hex"),
      mode: 0o644,
    });
    const chunk = frames[1]!;
    if (chunk.case !== "chunk") throw new Error("expected chunk second");
    expect(chunk.value).toEqual(content);
  });

  test("an empty file sends the metadata frame only", async () => {
    const { stagingClient, frames } = client();
    const results = await stageFiles(stagingClient, "session-1", [
      { path: "/workspace/.review/empty.json", content: new Uint8Array(), mode: 0o644 },
    ]);
    expect(results[0]!.ok).toBe(true);
    expect(frames).toHaveLength(1);
    expect(frames[0]!.case).toBe("metadata");
  });

  test("captures per-file errors and keeps staging the rest", async () => {
    const { stagingClient } = client({ failPaths: ["/workspace/b"] });
    const results = await stageFiles(stagingClient, "session-1", [
      { path: "/workspace/a", content: text("a"), mode: 0o644 },
      { path: "/workspace/b", content: text("b"), mode: 0o644 },
      { path: "/workspace/c", content: text("c"), mode: 0o600 },
    ]);
    expect(results).toEqual([
      { path: "/workspace/a", ok: true },
      { path: "/workspace/b", ok: false, error: "agentd rejected /workspace/b" },
      { path: "/workspace/c", ok: true },
    ]);
  });
});
