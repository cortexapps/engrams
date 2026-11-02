import type { makeManagedLifecycleStore } from "./managed-lifecycle-store.ts";
import type { makeManagedSessionRuntime } from "./managed-runtime.ts";
import { log } from "../log.ts";

/** Durable keyset scan. A failed row cannot starve later reservations. */
export function makeManagedSessionScanner(deps: {
  store: Pick<ReturnType<typeof makeManagedLifecycleStore>, "pending">;
  runtime: Pick<ReturnType<typeof makeManagedSessionRuntime>, "runOnce">;
}) {
  let after: string | undefined;
  let stopped = true;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let running: Promise<void> | undefined;
  async function runOnce(stopBetweenRows = false) {
    const rows = await deps.store.pending(after);
    for (const row of rows) {
      if (stopBetweenRows && stopped) break;
      try {
        await deps.runtime.runOnce(row.sessionId);
      } catch {
        log.warn("MCP managed session reconciliation unavailable");
      }
      after = row.sessionId;
    }
    if (rows.length === 0) after = undefined;
  }
  async function tick() {
    try {
      await runOnce(true);
    } catch {
      log.warn("MCP managed session scan unavailable");
    }
    if (!stopped)
      timer = setTimeout(() => {
        running = tick();
      }, 1000);
  }
  return {
    runOnce: () => runOnce(),
    async start() {
      if (!stopped) return;
      stopped = false;
      running = tick();
      await running;
    },
    async stop() {
      stopped = true;
      clearTimeout(timer);
      await running;
    },
  };
}
