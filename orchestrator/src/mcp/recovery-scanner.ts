import { makeMcpJournal } from "./journal.ts";
import { type McpRecovery, type McpExecutionMarker, mcpRecovery } from "./recovery.ts";
import { startMcpWorkflow } from "../workflows/mcp-exec.ts";
import { log } from "../log.ts";

/** Bounded database maintenance, with a keyset cursor so parked calls cannot starve later work. */
export function makeMcpRecoveryScanner(deps: {
  deploymentId: string;
  journal?: ReturnType<typeof makeMcpJournal>;
  runtime?: () => McpRecovery | undefined;
  startWorkflow?: (marker: McpExecutionMarker, workflowId: string) => Promise<void>;
  now?: () => Date;
}) {
  let after: string | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let running: Promise<void> | undefined;
  let stopped = true;
  let lifecycleTimer: ReturnType<typeof setTimeout> | undefined;
  let lifecycleRunning: Promise<void> | undefined;
  async function runOnce() {
    const journal = deps.journal ?? makeMcpJournal();
    const now = (deps.now ?? (() => new Date()))();
    // These producers recover distinct crash states. Disabling dispatch never disables expiry.
    await journal.expire(deps.deploymentId, now);
    await journal.expireUnadmitted(now);
    const runtime = (deps.runtime ?? mcpRecovery)();
    if (!runtime) return;
    const rows = await runtime.pending(after);
    for (const { workflowId, ...marker } of rows) {
      await (deps.startWorkflow ?? startMcpWorkflow)(marker, workflowId);
      after = marker.executionId;
    }
    if (rows.length === 0) after = undefined;
  }
  async function tick() {
    try {
      await runOnce();
    } catch {
      log.warn("MCP recovery scan unavailable");
    }
    if (!stopped)
      timer = setTimeout(() => {
        running = tick();
      }, 1_000);
  }
  async function runLifecycleOnce() {
    await (deps.runtime ?? mcpRecovery)()?.reconcileLifecycle();
  }
  async function lifecycleTick() {
    try {
      await runLifecycleOnce();
    } catch {
      log.warn("MCP lifecycle reconciliation unavailable");
    }
    if (!stopped)
      lifecycleTimer = setTimeout(() => {
        lifecycleRunning = lifecycleTick();
      }, 1_000);
  }
  return {
    runOnce,
    runLifecycleOnce,
    async start() {
      if (!stopped) return;
      stopped = false;
      running = tick();
      lifecycleRunning = lifecycleTick();
      await running;
    },
    async stop() {
      stopped = true;
      clearTimeout(timer);
      clearTimeout(lifecycleTimer);
      await running;
      await lifecycleRunning;
    },
  };
}
