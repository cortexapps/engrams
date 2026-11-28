import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { task, user } from "../db/schema.ts";
import { coldFixture } from "./mcp-cold-fixture.ts";
import { makeMcpRuntime } from "../mcp/runtime.ts";
import { makeMcpRecoveryScanner } from "../mcp/recovery-scanner.ts";
import { effectFixture } from "./mcp-effect-fixture.ts";
import { operatorPublication } from "./mcp-binding-fixture.ts";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";
await verifyMcpTestDatabases();

test("actual startup composition retains committed completion through missing and disabled configuration", async () => {
  const f = await effectFixture({ classification: "read" });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    expect((await f.driver.driveOnce(call.id)).state).toBe("succeeded");
    let delivered = 0,
      fail = true,
      bridgeConstructed = 0;
    const common = {
      db: f.db,
      deploymentId: crypto.randomUUID(),
      enabled: () => false,
      bridge: () => {
        bridgeConstructed++;
        return {
          readManagedCreationState: async () => {
            throw Error("unexpected creation read");
          },
          prepareManagedBoot: async () => {
            throw Error("unexpected prepare");
          },
          recoverManagedBoot: async () => {
            throw Error("unexpected recovery");
          },
        };
      },
      completer: {
        async completeToolCall(input: { resultJson: string }) {
          if (fail) throw Error("reply unavailable");
          expect(JSON.parse(input.resultJson).outcome).toBe("succeeded");
          delivered++;
        },
      },
    };
    let runtime = makeMcpRuntime(common);
    expect(runtime.services.enabled).toBe(false);
    expect(runtime.capabilities).toBeUndefined();
    expect(bridgeConstructed).toBe(0);
    const scanner = makeMcpRecoveryScanner({
      deploymentId: common.deploymentId,
      journal: runtime.services.journal,
      runtime: () => runtime.recovery,
      startWorkflow: async (marker) => {
        if (marker.sessionId === f.sessionId) await runtime.recovery.runOnce(marker);
      },
    });
    await expect(scanner.runOnce()).rejects.toThrow("completion unavailable");
    expect((await f.journal.get(call.id))!.completionEnqueuedVersion).toBe(0);
    runtime = makeMcpRuntime({
      ...common,
      configuration: {
        infrastructure: operatorPublication,
        control: f.remote.workerTls,
        workers: new Map([
          ["public", { enabled: false, deploymentId: f.deploymentId, tls: f.remote.workerTls }],
        ]),
        registrations: [],
      },
    });
    expect(runtime.services.enabled).toBe(false);
    expect(runtime.capabilities).toBeDefined();
    expect(bridgeConstructed).toBe(1);
    fail = false;
    await scanner.runOnce();
    expect(delivered).toBe(1);
    await expect(runtime.recovery.accept(f.request)).rejects.toMatchObject({ code: "disabled" });
    expect((await f.command("stats")).calls).toBe(1);
    await scanner.stop();
    const cold = await coldFixture(f, f.operatorPolicies);
    if (!runtime.validate) throw Error("configured runtime validation missing");
    await f.db.update(user).set({ banned: true }).where(eq(user.id, f.actorId));
    await expect(runtime.validate(cold.snapshot, f.sessionId)).rejects.toThrow(
      "creation principal unavailable",
    );
    await f.db.update(user).set({ banned: false }).where(eq(user.id, f.actorId));
    await f.db.update(task).set({ createdByUserId: null }).where(eq(task.id, f.taskId));
    await expect(runtime.validate(cold.snapshot, f.sessionId)).rejects.toThrow(
      "creation principal unavailable",
    );
  } finally {
    await f.cleanup();
  }
}, 20000);
