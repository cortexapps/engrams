import { describe, expect, test } from "bun:test";
import { EventEmitter } from "node:events";

import { listenForSpecChannel } from "./channel-listener.ts";

class FakeClient extends EventEmitter {
  readonly queries: string[] = [];
  readonly releases: boolean[] = [];
  listenError: Error | null = null;

  async query(queryText: string): Promise<unknown> {
    this.queries.push(queryText);
    if (queryText.startsWith("LISTEN") && this.listenError) throw this.listenError;
    return undefined;
  }

  release(destroy = false): void {
    this.releases.push(destroy);
  }
}

describe("the spec PostgreSQL channel listener", () => {
  test("reconnects after an error, re-LISTENs, and cleans up", async () => {
    const first = new FakeClient();
    const second = new FakeClient();
    const clients = [first, second];
    const warnings: string[] = [];
    const notifications: string[] = [];
    let reconnects = 0;
    const delays: Array<{
      reject(error: Error): void;
      resolve(): void;
    }> = [];
    const pool = {
      connect: async () => {
        const client = clients.shift();
        if (!client) throw new Error("no fake client is available");
        return client;
      },
    };
    const stop = await listenForSpecChannel(
      pool,
      "spec_update",
      (message) => notifications.push(message.payload ?? ""),
      {
        label: "Test listener",
        onWarning: (message) => warnings.push(message),
        onReconnect: () => {
          reconnects += 1;
        },
        delay: async (_milliseconds, signal) => {
          await new Promise<void>((resolve, reject) => {
            const finish = () => resolve();
            signal.addEventListener("abort", finish, { once: true });
            delays.push({
              resolve: () => {
                signal.removeEventListener("abort", finish);
                resolve();
              },
              reject: (error) => {
                signal.removeEventListener("abort", finish);
                reject(error);
              },
            });
          });
        },
      },
    );

    expect(first.queries).toEqual(["LISTEN spec_update"]);
    first.emit("error", new Error("connection dropped"));
    await eventually(() => delays.length === 1);
    delays.shift()!.reject(new Error("test timer failed"));
    await eventually(() => delays.length === 1);
    delays.shift()!.resolve();
    await eventually(() => reconnects === 1);

    second.emit("notification", { channel: "spec_update", payload: "wake" });
    expect(notifications).toEqual(["wake"]);
    expect(first.releases).toEqual([true]);
    expect(second.queries).toEqual(["LISTEN spec_update"]);
    expect(warnings).toEqual([
      "Test listener failed: connection dropped; reconnecting",
      "Test listener reconnect delay failed: test timer failed",
    ]);

    await stop();
    expect(second.queries).toEqual(["LISTEN spec_update", "UNLISTEN spec_update"]);
    expect(second.releases).toEqual([false]);
    expect(first.listenerCount("error")).toBe(0);
    expect(second.listenerCount("error")).toBe(0);
  });

  test("releases a client when the initial LISTEN fails", async () => {
    const client = new FakeClient();
    client.listenError = new Error("LISTEN failed");

    await expect(
      listenForSpecChannel({ connect: async () => client }, "spec_update", () => {}),
    ).rejects.toThrow("LISTEN failed");
    expect(client.releases).toEqual([true]);
    expect(client.listenerCount("notification")).toBe(0);
    expect(client.listenerCount("error")).toBe(0);
  });
});

async function eventually(done: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    if (done()) return;
    await Bun.sleep(1);
  }
  throw new Error("The expected listener state did not occur");
}
