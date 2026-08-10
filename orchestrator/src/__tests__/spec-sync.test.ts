import { afterEach, describe, expect, test } from "bun:test";
import { EventEmitter } from "node:events";
import type { AddressInfo } from "node:net";
import { Hono } from "hono";
import { createNodeWebSocket } from "@hono/node-ws";
import { WebSocket as WebSocketClient, type RawData } from "ws";
import * as awarenessProtocol from "y-protocols/awareness";
import * as Y from "yjs";

import { buildServer } from "../server.ts";
import {
  makeSpecSyncUpgradeHandler,
  parseSpecSyncPath,
  SpecSyncHub,
  type SpecAwarenessBus,
  type SpecParticipantStore,
  type SpecSyncDocuments,
  type SpecSyncTimers,
} from "../routes/spec-sync.ts";
import {
  decodeSpecSyncMessage,
  encodeAwarenessState,
  encodeSyncUpdate,
} from "../routes/spec-sync-protocol.ts";

const cleanups: Array<() => void | Promise<void>> = [];
const SPEC_ONE = "00000000-0000-4000-8000-000000000101";
const SPEC_SHARED = "00000000-0000-4000-8000-000000000102";
const SPEC_RETIRED = "00000000-0000-4000-8000-000000000103";

afterEach(async () => {
  for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

function fakeDocuments(): SpecSyncDocuments {
  const doc = new Y.Doc();
  const listeners = new Set<(event: { update: Uint8Array }) => void>();
  return {
    loadDoc: async () => ({ doc }),
    applyUpdate: async (_specId, update) => {
      Y.applyUpdate(doc, update);
      for (const listener of listeners) listener({ update });
    },
    subscribe: (_specId, listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    evict: () => {},
  };
}

function fakeAwarenessBus(): SpecAwarenessBus {
  return {
    start: async () => async () => {},
    publish: async () => {},
    query: async () => {},
    publishParticipantConnected: async () => {},
  };
}

async function listenForSpecSync(input: {
  member: boolean;
  authenticated?: boolean;
  participants?: SpecParticipantStore;
  documents?: SpecSyncDocuments;
  awarenessBus?: SpecAwarenessBus;
  onWarning?: (message: string) => void;
  membershipError?: Error;
}) {
  const app = new Hono();
  const nodeWs = createNodeWebSocket({ app });
  const participants: SpecParticipantStore = input.participants ?? {
    connect: async () => 1n,
    renew: async () => true,
    disconnect: async () => {},
  };
  const deps = {
    documents: input.documents ?? fakeDocuments(),
    participants,
    awarenessBus: input.awarenessBus ?? fakeAwarenessBus(),
    getSession: async () =>
      input.authenticated === false
        ? null
        : { user: { id: "member-not-owner", name: "Taylor Member" } },
    resolveMembership: async () => {
      if (input.membershipError) throw input.membershipError;
      return input.member;
    },
    onWarning: input.onWarning,
  };
  const hub = new SpecSyncHub(deps);
  await hub.start();
  const server = buildServer(app, () => {}, nodeWs, [makeSpecSyncUpgradeHandler(deps, hub)]);
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  cleanups.push(async () => {
    await hub.stop();
    server.closeAllConnections();
    server.close();
  });
  return (server.address() as AddressInfo).port;
}

function waitForClose(client: WebSocketClient): Promise<number> {
  cleanups.push(() => client.close());
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error("no WebSocket close within 5s")), 5_000);
    client.on("close", (code) => {
      clearTimeout(timeout);
      resolve(code);
    });
    client.on("error", () => {
      // The close event carries the application rejection code.
    });
  });
}

describe("the spec sync UpgradeHook", () => {
  test("parses only the dedicated route and a valid Yjs client id", () => {
    expect(
      parseSpecSyncPath(new URL(`http://localhost/api/v1/specs/${SPEC_ONE}/sync?clientId=42`)),
    ).toEqual({ specId: SPEC_ONE, clientId: "42" });
    expect(parseSpecSyncPath(new URL(`http://localhost/api/v1/specs/${SPEC_ONE}/sync`))).toEqual({
      specId: SPEC_ONE,
      clientId: null,
    });
    expect(parseSpecSyncPath(new URL("http://localhost/api/v1/specs/spec-1/publish"))).toBeNull();
  });

  test("rejects malformed encoding and non-UUID spec ids without throwing", async () => {
    expect(parseSpecSyncPath(new URL("http://localhost/api/v1/specs/%/sync?clientId=42"))).toEqual({
      specId: null,
      clientId: null,
    });
    expect(
      parseSpecSyncPath(new URL("http://localhost/api/v1/specs/not-a-uuid/sync?clientId=42")),
    ).toEqual({ specId: null, clientId: null });

    const port = await listenForSpecSync({ member: true });
    const client = new WebSocketClient(
      `ws://127.0.0.1:${port}/api/v1/specs/not-a-uuid/sync?clientId=42`,
    );
    expect(await waitForClose(client)).toBe(4400);
  });

  test("closes the socket when the upgrade guard fails unexpectedly", async () => {
    const port = await listenForSpecSync({
      member: true,
      membershipError: new Error("membership store unavailable"),
    });
    const client = new WebSocketClient(
      `ws://127.0.0.1:${port}/api/v1/specs/${SPEC_ONE}/sync?clientId=42`,
    );
    expect(await waitForClose(client)).toBe(4500);
  });

  test("admits an organization member who is not the owner", async () => {
    const connected: Array<[string, string, string]> = [];
    const disconnected: Array<[string, string]> = [];
    const port = await listenForSpecSync({
      member: true,
      participants: {
        connect: async (specId, clientId, userId) => {
          connected.push([specId, clientId, userId]);
          return 1n;
        },
        renew: async () => true,
        disconnect: async (specId, clientId) => {
          disconnected.push([specId, clientId]);
        },
      },
    });
    const client = new WebSocketClient(
      `ws://127.0.0.1:${port}/api/v1/specs/${SPEC_ONE}/sync?clientId=42`,
    );
    cleanups.push(() => client.close());

    await new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error("no sync frame within 5s")), 5_000);
      client.once("message", () => {
        clearTimeout(timeout);
        resolve();
      });
      client.once("error", reject);
    });
    expect(connected).toEqual([[SPEC_ONE, "42", "member-not-owner"]]);
    client.close();
    await new Promise<void>((resolve) => client.once("close", () => resolve()));
    await eventually(() => disconnected.length === 1);
    expect(disconnected).toEqual([[SPEC_ONE, "42"]]);
  });

  test("rejects a non-member with close code 4404", async () => {
    const port = await listenForSpecSync({ member: false });
    const client = new WebSocketClient(
      `ws://127.0.0.1:${port}/api/v1/specs/${SPEC_ONE}/sync?clientId=42`,
    );
    expect(await waitForClose(client)).toBe(4404);
  });

  test("rejects an unauthenticated caller with 4401 and a missing client id with 4400", async () => {
    const unauthenticatedPort = await listenForSpecSync({
      member: true,
      authenticated: false,
    });
    const unauthenticated = new WebSocketClient(
      `ws://127.0.0.1:${unauthenticatedPort}/api/v1/specs/${SPEC_ONE}/sync?clientId=42`,
    );
    expect(await waitForClose(unauthenticated)).toBe(4401);

    const memberPort = await listenForSpecSync({ member: true });
    const missingClientId = new WebSocketClient(
      `ws://127.0.0.1:${memberPort}/api/v1/specs/${SPEC_ONE}/sync`,
    );
    expect(await waitForClose(missingClientId)).toBe(4400);
  });

  test("relays document and awareness updates between two replicas", async () => {
    const documents = fakeDocumentNetwork();
    const awareness = fakeAwarenessNetwork();
    const firstPort = await listenForSpecSync({
      member: true,
      documents: documents.replica(),
      awarenessBus: awareness.replica(),
    });
    const secondPort = await listenForSpecSync({
      member: true,
      documents: documents.replica(),
      awarenessBus: awareness.replica(),
    });
    const firstDocument = new Y.Doc();
    const secondDocument = new Y.Doc();
    const firstClient = new WebSocketClient(
      `ws://127.0.0.1:${firstPort}/api/v1/specs/${SPEC_SHARED}/sync?clientId=${firstDocument.clientID}`,
    );
    const secondClient = new WebSocketClient(
      `ws://127.0.0.1:${secondPort}/api/v1/specs/${SPEC_SHARED}/sync?clientId=${secondDocument.clientID}`,
    );
    const firstReady = waitForDecodedMessage(firstClient, "sync-step-1");
    const secondReady = waitForDecodedMessage(secondClient, "sync-step-1");
    cleanups.push(() => {
      firstClient.close();
      secondClient.close();
      firstDocument.destroy();
      secondDocument.destroy();
    });
    await Promise.all([
      waitForOpen(firstClient),
      waitForOpen(secondClient),
      firstReady,
      secondReady,
    ]);

    const relayedDocument = waitForDecodedMessage(secondClient, "sync-update");
    let localUpdate: Uint8Array | undefined;
    firstDocument.once("update", (update: Uint8Array) => {
      localUpdate = update;
    });
    firstDocument.getMap("content").set("title", "Shared title");
    if (!localUpdate) throw new Error("the local document did not produce an update");
    firstClient.send(encodeSyncUpdate(localUpdate));
    const documentMessage = await relayedDocument;
    if (documentMessage.kind !== "sync-update") throw new Error("expected a document update");
    Y.applyUpdate(secondDocument, documentMessage.update);
    expect(secondDocument.getMap("content").get("title")).toBe("Shared title");

    const relayedAwareness = waitForDecodedMessage(secondClient, "awareness");
    const firstAwareness = new awarenessProtocol.Awareness(firstDocument);
    firstAwareness.setLocalState({ user: { id: "forged", name: "Forged name" } });
    firstClient.send(encodeAwarenessState(firstAwareness));
    const awarenessMessage = await relayedAwareness;
    if (awarenessMessage.kind !== "awareness") throw new Error("expected an awareness update");
    const secondAwareness = new awarenessProtocol.Awareness(secondDocument);
    awarenessProtocol.applyAwarenessUpdate(secondAwareness, awarenessMessage.update, "test");
    expect(secondAwareness.getStates().get(firstDocument.clientID)?.user).toMatchObject({
      id: "member-not-owner",
      name: "Taylor Member",
    });
    firstAwareness.destroy();
    secondAwareness.destroy();
  });

  test("supersedes an older same-client socket on another replica", async () => {
    const awareness = fakeAwarenessNetwork();
    const oldSocket = new DeferredCloseSpecSocket();
    const newSocket = new SilentSpecSocket();
    const intendedSocket = new SilentSpecSocket();
    let epoch = 0n;
    let visibleEpoch: bigint | null = null;
    const readVisibleEpoch = (): bigint | null => visibleEpoch;
    const disconnects: bigint[] = [];
    const participants: SpecParticipantStore = {
      connect: async () => {
        epoch += 1n;
        visibleEpoch = epoch;
        return epoch;
      },
      renew: async (_specId, _clientId, candidate) => visibleEpoch === candidate,
      disconnect: async (_specId, _clientId, candidate) => {
        disconnects.push(candidate);
        if (visibleEpoch === candidate) visibleEpoch = null;
      },
    };
    const oldHub = new SpecSyncHub({
      documents: fakeDocuments(),
      participants,
      awarenessBus: awareness.replica(),
    });
    const newHub = new SpecSyncHub({
      documents: fakeDocuments(),
      participants,
      awarenessBus: awareness.replica(),
    });
    await oldHub.start();
    await newHub.start();
    cleanups.push(async () => {
      await oldHub.stop();
      await newHub.stop();
    });

    await oldHub.connect(SPEC_SHARED, "42", { id: "member" }, oldSocket);
    await newHub.connect(SPEC_SHARED, "42", { id: "member" }, newSocket);
    await eventually(() => oldSocket.closeCodes[0] === 4009);

    newSocket.close();
    await eventually(() => disconnects.includes(2n));
    await oldHub.connect(SPEC_SHARED, "42", { id: "member" }, intendedSocket);
    oldSocket.finishClose();

    expect(oldSocket.closeCodes).toEqual([4009]);
    expect(disconnects).toEqual([1n, 2n]);
    expect(readVisibleEpoch()).toBe(3n);
    expect(intendedSocket.readyState).toBe(WebSocketClient.OPEN);
  });

  test("retires an empty room and reports a failed participant disconnect", async () => {
    let loads = 0;
    let unsubscribes = 0;
    let evictions = 0;
    const warnings: string[] = [];
    const documents: SpecSyncDocuments = {
      loadDoc: async () => {
        loads += 1;
        return { doc: new Y.Doc() };
      },
      applyUpdate: async () => {},
      subscribe: () => () => {
        unsubscribes += 1;
      },
      evict: () => {
        evictions += 1;
      },
    };
    const port = await listenForSpecSync({
      member: true,
      documents,
      participants: {
        connect: async () => 1n,
        renew: async () => true,
        disconnect: async () => {
          throw new Error("participant store unavailable");
        },
      },
      onWarning: (message) => warnings.push(message),
    });
    const first = new WebSocketClient(
      `ws://127.0.0.1:${port}/api/v1/specs/${SPEC_RETIRED}/sync?clientId=42`,
    );
    cleanups.push(() => first.close());
    await waitForDecodedMessage(first, "sync-step-1");
    first.close();
    await new Promise<void>((resolve) => first.once("close", () => resolve()));
    await eventually(() => warnings.length === 1 && unsubscribes === 1 && evictions === 1);

    const second = new WebSocketClient(
      `ws://127.0.0.1:${port}/api/v1/specs/${SPEC_RETIRED}/sync?clientId=43`,
    );
    cleanups.push(() => second.close());
    await waitForDecodedMessage(second, "sync-step-1");
    expect(loads).toBe(2);
    expect(warnings).toEqual([
      `Disconnect participant from spec ${SPEC_RETIRED} failed: participant store unavailable`,
    ]);
  });

  test("retires a half-dead socket after it misses a heartbeat pong", async () => {
    const timers = new ManualTimers();
    const socket = new SilentSpecSocket();
    let disconnects = 0;
    let unsubscribes = 0;
    const hub = new SpecSyncHub({
      documents: {
        loadDoc: async () => ({ doc: new Y.Doc() }),
        applyUpdate: async () => {},
        subscribe: () => () => {
          unsubscribes += 1;
        },
        evict: () => {},
      },
      participants: {
        connect: async () => 1n,
        renew: async () => true,
        disconnect: async () => {
          disconnects += 1;
        },
      },
      awarenessBus: fakeAwarenessBus(),
      heartbeatIntervalMs: 10,
      timers,
    });
    cleanups.push(() => hub.stop());

    await hub.connect(SPEC_ONE, "42", { id: "member" }, socket);
    expect(timers.size).toBe(1);
    timers.tick();
    expect(socket.pings).toBe(1);
    expect(socket.terminations).toBe(0);
    timers.tick();

    await eventually(() => disconnects === 1 && unsubscribes === 1);
    expect(socket.terminations).toBe(1);
    expect(timers.size).toBe(0);
  });

  test("renews the current participant epoch after a heartbeat pong", async () => {
    const timers = new ManualTimers();
    const socket = new SilentSpecSocket();
    const renewals: Array<[string, string, bigint]> = [];
    const hub = new SpecSyncHub({
      documents: fakeDocuments(),
      participants: {
        connect: async () => 7n,
        renew: async (specId, clientId, epoch) => {
          renewals.push([specId, clientId, epoch]);
          return true;
        },
        disconnect: async () => {},
      },
      awarenessBus: fakeAwarenessBus(),
      heartbeatIntervalMs: 10,
      timers,
    });
    cleanups.push(() => hub.stop());

    await hub.connect(SPEC_ONE, "42", { id: "member" }, socket);
    timers.tick();
    socket.emit("pong");

    await eventually(() => renewals.length === 1);
    expect(renewals).toEqual([[SPEC_ONE, "42", 7n]]);
  });

  test("terminates a socket when its pong carries a stale participant epoch", async () => {
    const timers = new ManualTimers();
    const socket = new SilentSpecSocket();
    let disconnects = 0;
    const hub = new SpecSyncHub({
      documents: fakeDocuments(),
      participants: {
        connect: async () => 1n,
        renew: async () => false,
        disconnect: async () => {
          disconnects += 1;
        },
      },
      awarenessBus: fakeAwarenessBus(),
      heartbeatIntervalMs: 10,
      timers,
    });
    cleanups.push(() => hub.stop());

    await hub.connect(SPEC_ONE, "42", { id: "member" }, socket);
    timers.tick();
    socket.emit("pong");

    await eventually(() => socket.terminations === 1 && disconnects === 1);
    expect(timers.size).toBe(0);
  });

  test("supersedes an older same-client socket before its stale events can affect presence", async () => {
    const timers = new ManualTimers();
    const first = new DeferredCloseSpecSocket();
    const second = new SilentSpecSocket();
    const third = new SilentSpecSocket();
    const appliedUpdates: number[] = [];
    const renewals: bigint[] = [];
    const disconnects: bigint[] = [];
    let nextEpoch = 0n;
    let visibleEpoch: bigint | null = null;
    const readVisibleEpoch = (): bigint | null => visibleEpoch;
    let finishSecondDisconnect = () => {};
    const secondDisconnectGate = new Promise<void>((resolve) => {
      finishSecondDisconnect = resolve;
    });
    const hub = new SpecSyncHub({
      documents: {
        loadDoc: async () => ({ doc: new Y.Doc() }),
        applyUpdate: async (_specId, update) => {
          appliedUpdates.push(update[0] ?? -1);
        },
        subscribe: () => () => {},
        evict: () => {},
      },
      participants: {
        connect: async () => {
          nextEpoch += 1n;
          visibleEpoch = nextEpoch;
          return nextEpoch;
        },
        renew: async (_specId, _clientId, epoch) => {
          renewals.push(epoch);
          return visibleEpoch === epoch;
        },
        disconnect: async (_specId, _clientId, epoch) => {
          disconnects.push(epoch);
          if (epoch === 2n) await secondDisconnectGate;
          if (visibleEpoch === epoch) visibleEpoch = null;
        },
      },
      awarenessBus: fakeAwarenessBus(),
      heartbeatIntervalMs: 10,
      timers,
    });
    cleanups.push(() => hub.stop());

    await hub.connect(SPEC_ONE, "42", { id: "member" }, first);
    await hub.connect(SPEC_ONE, "42", { id: "member" }, second);

    expect(first.closeCodes).toEqual([4009]);
    expect(timers.size).toBe(1);
    first.emit("message", encodeSyncUpdate(new Uint8Array([1])), true);
    second.emit("message", encodeSyncUpdate(new Uint8Array([2])), true);
    await eventually(() => appliedUpdates.length === 1);
    expect(appliedUpdates).toEqual([2]);

    second.close();
    await eventually(() => disconnects.includes(2n));
    const thirdConnection = hub.connect(SPEC_ONE, "42", { id: "member" }, third);
    await thirdConnection;
    finishSecondDisconnect();
    first.emit("pong");
    first.finishClose();
    timers.tick();
    third.emit("pong");

    await eventually(() => renewals.includes(3n));
    expect(renewals).toEqual([3n]);
    expect(disconnects).toEqual([1n, 2n]);
    expect(readVisibleEpoch()).toBe(3n);
    expect(timers.size).toBe(1);
    expect(third.pings).toBe(1);
  });

  test("retires a socket that emits an error", async () => {
    const socket = new SilentSpecSocket();
    const warnings: string[] = [];
    let disconnects = 0;
    let unsubscribes = 0;
    const hub = new SpecSyncHub({
      documents: {
        loadDoc: async () => ({ doc: new Y.Doc() }),
        applyUpdate: async () => {},
        subscribe: () => () => {
          unsubscribes += 1;
        },
        evict: () => {},
      },
      participants: {
        connect: async () => 1n,
        renew: async () => true,
        disconnect: async () => {
          disconnects += 1;
        },
      },
      awarenessBus: fakeAwarenessBus(),
      onWarning: (message) => warnings.push(message),
    });
    cleanups.push(() => hub.stop());

    await hub.connect(SPEC_ONE, "42", { id: "member" }, socket);
    socket.emit("error", new Error("socket write failed"));

    await eventually(() => disconnects === 1 && unsubscribes === 1);
    expect(socket.terminations).toBe(1);
    expect(warnings).toEqual([`Spec sync socket failed for spec ${SPEC_ONE}: socket write failed`]);
    expect(socket.listenerCount("error")).toBe(0);
  });

  test("does not join a socket that closes during the participant bind", async () => {
    const timers = new ManualTimers();
    const socket = new SilentSpecSocket();
    let connectStarted = false;
    let finishConnect = () => {};
    let disconnects = 0;
    let unsubscribes = 0;
    const participantGate = new Promise<void>((resolve) => {
      finishConnect = resolve;
    });
    const hub = new SpecSyncHub({
      documents: {
        loadDoc: async () => ({ doc: new Y.Doc() }),
        applyUpdate: async () => {},
        subscribe: () => () => {
          unsubscribes += 1;
        },
        evict: () => {},
      },
      participants: {
        connect: async () => {
          connectStarted = true;
          await participantGate;
          return 1n;
        },
        renew: async () => true,
        disconnect: async () => {
          disconnects += 1;
        },
      },
      awarenessBus: fakeAwarenessBus(),
      heartbeatIntervalMs: 10,
      timers,
    });
    cleanups.push(() => hub.stop());

    const connecting = hub.connect(SPEC_ONE, "42", { id: "member" }, socket);
    await eventually(() => connectStarted);
    socket.close();
    finishConnect();
    await connecting;

    expect(disconnects).toBe(1);
    expect(unsubscribes).toBe(1);
    expect(timers.size).toBe(0);
  });
});

describe("SpecSyncHub awareness failures", () => {
  test("queries peers for active rooms after the awareness listener reconnects", async () => {
    let reconnect: (() => void) | undefined;
    const queries: string[] = [];
    const awarenessBus: SpecAwarenessBus = {
      start: async (handlers) => {
        reconnect = handlers.reconnect;
        return async () => {};
      },
      publish: async () => {},
      query: async (specId) => {
        queries.push(specId);
      },
      publishParticipantConnected: async () => {},
    };
    const hub = new SpecSyncHub({
      documents: fakeDocuments(),
      participants: {
        connect: async () => 1n,
        renew: async () => true,
        disconnect: async () => {},
      },
      awarenessBus,
    });
    await hub.start();
    cleanups.push(() => hub.stop());
    await hub.enter({
      specId: SPEC_ONE,
      sessionId: "session-1",
      toolCallId: "tool-1",
      sectionId: "requirements",
    });

    reconnect?.();
    await eventually(() => queries.length === 1);
    expect(queries).toEqual([SPEC_ONE]);
  });

  test("reports failed local publishes and peer tasks", async () => {
    const warnings: string[] = [];
    let handlers:
      | {
          update(specId: string, update: Uint8Array): void;
          query(specId: string): void;
          participantConnected(specId: string, clientId: string, epoch: bigint): void;
        }
      | undefined;
    const awarenessBus: SpecAwarenessBus = {
      start: async (nextHandlers) => {
        handlers = nextHandlers;
        return async () => {};
      },
      publish: async () => {
        throw new Error("relay unavailable");
      },
      query: async () => {},
      publishParticipantConnected: async () => {
        throw new Error("relay unavailable");
      },
    };
    const hub = new SpecSyncHub({
      documents: fakeDocuments(),
      participants: {
        connect: async () => 1n,
        renew: async () => true,
        disconnect: async () => {},
      },
      awarenessBus,
      onWarning: (message) => warnings.push(message),
    });
    await hub.start();
    cleanups.push(() => hub.stop());

    await hub.enter({
      specId: "spec-1",
      sessionId: "session-1",
      toolCallId: "tool-1",
      sectionId: "failure-modes",
    });
    handlers?.update("spec-1", new Uint8Array([255]));
    handlers?.query("spec-1");

    await eventually(() => warnings.length === 3);
    expect(warnings).toContain("Publish awareness for spec spec-1 failed: relay unavailable");
    expect(warnings).toContain(
      "Apply peer awareness for spec spec-1 failed: Unexpected end of array",
    );
    expect(warnings).toContain("Answer awareness query for spec spec-1 failed: relay unavailable");
  });
});

async function eventually(done: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    if (done()) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error("expected condition did not become true");
}

class ManualTimers implements SpecSyncTimers {
  private readonly callbacks = new Map<unknown, () => void>();

  get size(): number {
    return this.callbacks.size;
  }

  setInterval(callback: () => void): unknown {
    const handle = {};
    this.callbacks.set(handle, callback);
    return handle;
  }

  clearInterval(handle: unknown): void {
    this.callbacks.delete(handle);
  }

  tick(): void {
    for (const callback of [...this.callbacks.values()]) callback();
  }
}

class SilentSpecSocket extends EventEmitter {
  readyState: number = WebSocketClient.OPEN;
  pings = 0;
  terminations = 0;

  send(_data: Uint8Array): void {}

  ping(): void {
    this.pings += 1;
  }

  terminate(): void {
    this.terminations += 1;
    this.readyState = WebSocketClient.CLOSED;
    this.emit("close");
  }

  close(): void {
    if (this.readyState === WebSocketClient.CLOSED) return;
    this.readyState = WebSocketClient.CLOSED;
    this.emit("close");
  }
}

class DeferredCloseSpecSocket extends SilentSpecSocket {
  readonly closeCodes: number[] = [];

  close(code = 1000): void {
    this.closeCodes.push(code);
  }

  finishClose(): void {
    if (this.readyState === WebSocketClient.CLOSED) return;
    this.readyState = WebSocketClient.CLOSED;
    this.emit("close");
  }
}

function fakeDocumentNetwork(): { replica(): SpecSyncDocuments } {
  const replicas: Array<{
    doc: Y.Doc;
    listeners: Set<(event: { update: Uint8Array }) => void>;
  }> = [];
  return {
    replica() {
      const record = {
        doc: new Y.Doc(),
        listeners: new Set<(event: { update: Uint8Array }) => void>(),
      };
      replicas.push(record);
      return {
        loadDoc: async () => ({ doc: record.doc }),
        applyUpdate: async (_specId, update) => {
          for (const replica of replicas) {
            Y.applyUpdate(replica.doc, update);
            for (const listener of replica.listeners) listener({ update });
          }
        },
        subscribe: (_specId, listener) => {
          record.listeners.add(listener);
          return () => record.listeners.delete(listener);
        },
        evict: () => {},
      };
    },
  };
}

function fakeAwarenessNetwork(): { replica(): SpecAwarenessBus } {
  const handlers = new Set<{
    update(specId: string, update: Uint8Array): void;
    query(specId: string): void;
    participantConnected(specId: string, clientId: string, epoch: bigint): void;
  }>();
  return {
    replica() {
      let localHandlers:
        | {
            update(specId: string, update: Uint8Array): void;
            query(specId: string): void;
            participantConnected(specId: string, clientId: string, epoch: bigint): void;
          }
        | undefined;
      return {
        start: async (nextHandlers) => {
          localHandlers = nextHandlers;
          handlers.add(nextHandlers);
          return async () => {
            if (localHandlers) handlers.delete(localHandlers);
          };
        },
        publish: async (specId, update) => {
          for (const handler of handlers) handler.update(specId, update);
        },
        query: async (specId) => {
          for (const handler of handlers) handler.query(specId);
        },
        publishParticipantConnected: async (specId, clientId, epoch) => {
          for (const handler of handlers) handler.participantConnected(specId, clientId, epoch);
        },
      };
    },
  };
}

function waitForOpen(client: WebSocketClient): Promise<void> {
  return new Promise((resolve, reject) => {
    if (client.readyState === WebSocketClient.OPEN) {
      resolve();
      return;
    }
    client.once("open", () => resolve());
    client.once("error", reject);
  });
}

function waitForDecodedMessage(
  client: WebSocketClient,
  expectedKind: ReturnType<typeof decodeSpecSyncMessage>["kind"],
): Promise<ReturnType<typeof decodeSpecSyncMessage>> {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(
      () => reject(new Error(`no ${expectedKind} message within 5s`)),
      5_000,
    );
    const onMessage = (data: RawData) => {
      const bytes =
        data instanceof ArrayBuffer
          ? new Uint8Array(data)
          : Array.isArray(data)
            ? Buffer.concat(data)
            : data;
      const message = decodeSpecSyncMessage(bytes);
      if (message.kind !== expectedKind) return;
      clearTimeout(timeout);
      client.off("message", onMessage);
      resolve(message);
    };
    client.on("message", onMessage);
  });
}
