import { expect, test } from "bun:test";
import { reviewContext } from "../mcp/journal-review.ts";
import { effectFixture } from "./mcp-effect-fixture.ts";

test("manual resolution cannot authorize a new write while the old intent can send", async () => {
  const f = await effectFixture({ classification: "write" });
  let unblock!: () => void;
  const blocked = new Promise<void>((resolve) => {
    unblock = resolve;
  });
  let notify!: () => void;
  const entered = new Promise<void>((resolve) => {
    notify = resolve;
  });
  try {
    const first = await f.driver.acceptToolRequest(f.request);
    await f.driver.driveOnce(first.id);
    const approval = await f.journal.requestApproval(first.id, f.deploymentId, new Date());
    await f.journal.decide(first.id, f.actorId, true, reviewContext(first, approval), new Date());
    let fences = 0;
    f.beforeFence(async () => {
      if (f.leases > 0 && ++fences === 2) {
        notify();
        await blocked;
      }
    });
    const pending = f.driver.driveOnce(first.id);
    await entered;
    const earlierRequest = { ...f.request, toolCallId: "already-accepted" };
    f.requests.set(earlierRequest.toolCallId, earlierRequest);
    const earlier = await f.driver.acceptToolRequest(earlierRequest);
    const dispatched = (await f.journal.get(first.id))!;
    const unknown = await f.journal.cancel(first.id, f.actorId, new Date(), dispatched.version);
    const context = {
      executionId: first.id,
      journalRevision: unknown.version,
      requestDigest: unknown.requestDigest,
      bindingHash: unknown.bindingHash,
      intentId: unknown.intentId!,
    };
    await f.journal.resolveUnknown(first.id, f.actorId, "acknowledged", new Date(), context);
    const earlierAttempt = await f.driver.driveOnce(earlier.id).then(
      () => "accepted",
      () => "blocked",
    );
    const next = { ...f.request, toolCallId: "new-call" };
    f.requests.set(next.toolCallId, next);
    const attempted = await f.driver.acceptToolRequest(next).then(
      () => "accepted",
      () => "blocked",
    );
    f.advanceNow(310_000);
    unblock();
    await pending;
    expect(attempted).toBe("blocked");
    expect(earlierAttempt).toBe("blocked");
    expect((await f.command("stats")).calls).toBe(0);
    const accepted = await f.driver.acceptToolRequest(next);
    expect(accepted.toolCallId).toBe("new-call");
  } finally {
    unblock();
    await f.cleanup();
  }
}, 20000);
