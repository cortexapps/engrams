import { expect, test } from "bun:test";
import { makeMcpActiveWatch } from "../mcp/active-watch.ts";

test("registers before its first authority read and aborts if it fails", async () => {
  let registered = false;
  const watch = makeMcpActiveWatch({
    check: async () => {
      expect(registered).toBe(true);
      throw Error("unavailable");
    },
    intervalMs: 10,
  });
  const active = watch.register("execution");
  registered = true;
  await active.ready;
  expect(active.signal.aborted).toBe(true);
  active.close();
});

test("periodic authority checks and local cancellation abort active work", async () => {
  let calls = 0;
  const watch = makeMcpActiveWatch({
    check: async () => {
      if (++calls > 1) throw Error("changed");
    },
    intervalMs: 10,
  });
  const active = watch.register("execution");
  await active.ready;
  await new Promise<void>((resolve) =>
    active.signal.addEventListener("abort", () => resolve(), { once: true }),
  );
  expect(calls).toBeGreaterThan(1);
  active.close();
  const other = watch.register("other");
  watch.cancel("other");
  expect(other.signal.aborted).toBe(true);
  other.close();
});

test("a stalled authority read aborts within the check budget", async () => {
  const watch = makeMcpActiveWatch({ check: () => new Promise<void>(() => {}), intervalMs: 10 });
  const active = watch.register("execution");
  await Promise.race([active.ready, new Promise<void>((resolve) => setTimeout(resolve, 50))]);
  expect(active.signal.aborted).toBe(true);
  active.close();
});
