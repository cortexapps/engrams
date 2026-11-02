import { expect, test } from "bun:test";
import { reviewFixture } from "./mcp-review-fixture.ts";

test("reads only the latest persisted draft and selected published revision", async () => {
  const f = await reviewFixture();
  try {
    const current = (await f.connections.get(f.connection.id))!;
    const newer = await f.repository.saveDraft(
      f.connection.id,
      current.mcp!,
      [{ upstreamName: "Read.New", description: "New", inputSchema: { type: "object" } }],
      f.actorId,
      new Date(f.now.getTime() + 1000),
    );
    const draft = await f.repository.getLatestDraft(f.connection.id);
    const published = await f.repository.getCatalog(f.connection.id, current.mcp!.catalogRevision);
    expect(draft?.id).toBe(newer.id);
    expect(draft?.tools[0]?.upstreamName).toBe("Read.New");
    expect(published?.id).toBe(f.catalog.id);
    expect(published?.revision).toBe(1);
    expect(await f.repository.getCatalog(f.connection.id, 2)).toBeNull();
  } finally {
    await f.cleanup();
  }
}, 20000);
