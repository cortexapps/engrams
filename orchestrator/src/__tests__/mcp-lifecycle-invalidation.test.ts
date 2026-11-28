import { expect, test } from "bun:test";
import { effectFixture } from "./mcp-effect-fixture.ts";
import { makeMcpLifecycleInvalidator } from "../mcp/lifecycle-invalidation.ts";

test("internal lifecycle cancellation commits before local abort", async () => {
  const f = await effectFixture({ classification: "read" });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    const invalidator = makeMcpLifecycleInvalidator({
      db: f.db,
      abort: async (id) => {
        expect((await f.journal.get(id))!.state).toBe("canceled");
      },
    });
    await invalidator(f.sessionId, "interrupt");
    expect((await f.journal.cancellationReceipt(call.id))?.cause).toBe("interrupt");
    expect((await f.command("stats")).calls).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);
