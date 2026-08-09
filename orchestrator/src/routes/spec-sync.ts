import type { IncomingMessage } from "node:http";
import type { Socket } from "node:net";
import { WebSocket, WebSocketServer, type RawData } from "ws";
import * as awarenessProtocol from "y-protocols/awareness";
import type * as Y from "yjs";

import type { GetSession, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";
import {
  awarenessClientIds,
  decodeSpecSyncMessage,
  encodeAwarenessState,
  encodeSyncStep1,
  encodeSyncStep2,
  encodeSyncUpdate,
  withAwarenessUser,
} from "./spec-sync-protocol.ts";

const SPEC_SYNC_PATH = /^\/api\/v1\/specs\/([^/]+)\/sync$/;
const PEER_AWARENESS = Symbol("peer-awareness");
const MAX_AWARENESS_UPDATE_BYTES = 5_800;

export interface SpecDocumentUpdate {
  update: Uint8Array;
}

export interface SpecSyncDocuments {
  loadDoc(specId: string): Promise<{ doc: Y.Doc }>;
  applyUpdate(specId: string, update: Uint8Array, clientId: string): Promise<unknown>;
  subscribe(specId: string, listener: (event: SpecDocumentUpdate) => void): () => void;
}

export interface SpecParticipantStore {
  connect(specId: string, clientId: string, userId: string): Promise<void>;
  disconnect(specId: string, clientId: string): Promise<void>;
}

export interface SpecAwarenessBus {
  start(handlers: {
    update(specId: string, update: Uint8Array): void;
    query(specId: string): void;
  }): Promise<() => Promise<void>>;
  publish(specId: string, update: Uint8Array): Promise<void>;
  query(specId: string): Promise<void>;
}

export interface SpecPresenceEnterInput {
  specId: string;
  sessionId: string;
  toolCallId: string;
  sectionId: string;
}

export interface SpecPresenceLeaveInput {
  specId: string;
  sessionId: string;
  toolCallId: string;
}

/** Seam used by spec tools to show section-level agent presence. */
export interface SpecPresence {
  enter(input: SpecPresenceEnterInput): Promise<void>;
  leave(input: SpecPresenceLeaveInput): Promise<void>;
}

export interface SpecSyncDeps {
  documents: SpecSyncDocuments;
  participants: SpecParticipantStore;
  awarenessBus: SpecAwarenessBus;
  resolveMembership: ResolveSpecMembership;
  getSession?: GetSession;
  onWarning?: (message: string) => void;
}

export interface ParsedSpecSyncPath {
  specId: string;
  clientId: string | null;
}

export function parseSpecSyncPath(url: URL): ParsedSpecSyncPath | null {
  const match = SPEC_SYNC_PATH.exec(url.pathname);
  if (!match) return null;
  const rawClientId = url.searchParams.get("clientId");
  if (!rawClientId || !/^\d+$/.test(rawClientId)) {
    return { specId: decodeURIComponent(match[1]!), clientId: null };
  }
  const clientId = Number(rawClientId);
  if (!Number.isSafeInteger(clientId) || clientId < 0 || clientId > 0xffff_ffff) {
    return { specId: decodeURIComponent(match[1]!), clientId: null };
  }
  return { specId: decodeURIComponent(match[1]!), clientId: String(clientId) };
}

interface AgentPresenceState extends SpecPresenceEnterInput {}

interface SpecRoom {
  doc: Y.Doc;
  awareness: awarenessProtocol.Awareness;
  sockets: Set<WebSocket>;
  socketClientIds: Map<WebSocket, string>;
  clientConnectionCounts: Map<string, number>;
  agents: Map<string, AgentPresenceState>;
  queriedPeers: boolean;
  unsubscribeDocument: () => void;
}

interface SpecRoomEntry {
  promise: Promise<SpecRoom>;
  pendingUsers: number;
}

interface AwarenessChange {
  added: number[];
  updated: number[];
  removed: number[];
}

/** Own the local sockets for all spec rooms on one orchestrator replica. */
export class SpecSyncHub implements SpecPresence {
  private readonly rooms = new Map<string, SpecRoomEntry>();
  private stopBus: (() => Promise<void>) | null = null;

  constructor(
    private readonly deps: Pick<
      SpecSyncDeps,
      "documents" | "participants" | "awarenessBus" | "onWarning"
    >,
  ) {}

  async start(): Promise<void> {
    if (this.stopBus) return;
    this.stopBus = await this.deps.awarenessBus.start({
      update: (specId, update) => {
        this.runBackgroundTask(
          `Apply peer awareness for spec ${specId}`,
          this.applyPeerAwareness(specId, update),
        );
      },
      query: (specId) => {
        this.runBackgroundTask(
          `Answer awareness query for spec ${specId}`,
          this.answerAwarenessQuery(specId),
        );
      },
    });
  }

  async stop(): Promise<void> {
    const stopBus = this.stopBus;
    this.stopBus = null;
    if (stopBus) await stopBus();
    for (const entry of this.rooms.values()) {
      const room = await entry.promise;
      for (const socket of room.sockets) socket.close(1001, "orchestrator stopping");
      room.unsubscribeDocument();
      room.awareness.destroy();
    }
    this.rooms.clear();
  }

  async connect(
    specId: string,
    clientId: string,
    user: { id: string; name?: string },
    socket: WebSocket,
  ): Promise<void> {
    const entry = this.acquireRoom(specId);
    let room: SpecRoom | undefined;
    try {
      room = await entry.promise;
      await this.deps.participants.connect(specId, clientId, user.id);
      room.sockets.add(socket);
      room.socketClientIds.set(socket, clientId);
      room.clientConnectionCounts.set(
        clientId,
        (room.clientConnectionCounts.get(clientId) ?? 0) + 1,
      );
    } finally {
      entry.pendingUsers -= 1;
      if (room) this.retireRoomIfIdle(specId, entry, room);
    }

    socket.on("message", (data, isBinary) => {
      if (!isBinary) {
        socket.close(1003, "binary messages required");
        return;
      }
      void this.receive(specId, room, socket, clientId, user, rawDataBytes(data)).catch(() => {
        socket.close(1003, "invalid spec sync message");
      });
    });
    socket.once("close", () => {
      this.runBackgroundTask(
        `Disconnect participant from spec ${specId}`,
        this.disconnect(specId, room, socket),
      );
    });

    socket.send(encodeSyncStep1(room.doc));
    if (room.awareness.getStates().size > 0) socket.send(encodeAwarenessState(room.awareness));
    if (!room.queriedPeers) {
      room.queriedPeers = true;
      await this.deps.awarenessBus.query(specId);
    }
  }

  async enter(input: SpecPresenceEnterInput): Promise<void> {
    const entry = this.acquireRoom(input.specId);
    let room: SpecRoom | undefined;
    try {
      room = await entry.promise;
      room.agents.set(agentKey(input), input);
      room.awareness.setLocalState({
        agentPresence: [...room.agents.values()].map(({ sessionId, toolCallId, sectionId }) => ({
          name: "engram",
          sessionId,
          toolCallId,
          sectionId,
        })),
      });
    } finally {
      entry.pendingUsers -= 1;
      if (room) this.retireRoomIfIdle(input.specId, entry, room);
    }
  }

  async leave(input: SpecPresenceLeaveInput): Promise<void> {
    const entry = this.rooms.get(input.specId);
    if (!entry) return;
    const room = await entry.promise;
    room.agents.delete(agentKey(input));
    if (room.agents.size === 0) {
      room.awareness.setLocalState(null);
    } else {
      room.awareness.setLocalState({
        agentPresence: [...room.agents.values()].map(({ sessionId, toolCallId, sectionId }) => ({
          name: "engram",
          sessionId,
          toolCallId,
          sectionId,
        })),
      });
    }
    this.retireRoomIfIdle(input.specId, entry, room);
  }

  private acquireRoom(specId: string): SpecRoomEntry {
    const existing = this.rooms.get(specId);
    if (existing) {
      existing.pendingUsers += 1;
      return existing;
    }
    const entry: SpecRoomEntry = {
      promise: this.createRoom(specId),
      pendingUsers: 1,
    };
    this.rooms.set(specId, entry);
    void entry.promise.catch(() => {
      if (this.rooms.get(specId) === entry) this.rooms.delete(specId);
    });
    return entry;
  }

  private async createRoom(specId: string): Promise<SpecRoom> {
    const { doc } = await this.deps.documents.loadDoc(specId);
    const awareness = new awarenessProtocol.Awareness(doc);
    awareness.setLocalState(null);
    const room: SpecRoom = {
      doc,
      awareness,
      sockets: new Set(),
      socketClientIds: new Map(),
      clientConnectionCounts: new Map(),
      agents: new Map(),
      queriedPeers: false,
      unsubscribeDocument: () => {},
    };

    room.unsubscribeDocument = this.deps.documents.subscribe(specId, (event) => {
      this.broadcast(room, encodeSyncUpdate(event.update));
    });
    awareness.on("update", ({ added, updated, removed }: AwarenessChange, origin: unknown) => {
      const clients = [...added, ...updated, ...removed];
      if (clients.length === 0) return;
      const update = awarenessProtocol.encodeAwarenessUpdate(awareness, clients);
      this.broadcast(room, encodeAwarenessState(awareness, clients));
      if (origin !== PEER_AWARENESS) {
        this.runBackgroundTask(
          `Publish awareness for spec ${specId}`,
          this.deps.awarenessBus.publish(specId, update),
        );
      }
    });
    return room;
  }

  private async receive(
    specId: string,
    room: SpecRoom,
    socket: WebSocket,
    clientId: string,
    user: { id: string; name?: string },
    bytes: Uint8Array,
  ): Promise<void> {
    const message = decodeSpecSyncMessage(bytes);
    if (message.kind === "sync-step-1") {
      socket.send(encodeSyncStep2(room.doc, message.stateVector));
      return;
    }
    if (message.kind === "sync-update") {
      await this.deps.documents.applyUpdate(specId, message.update, clientId);
      return;
    }
    if (message.kind === "awareness-query") {
      if (room.awareness.getStates().size > 0) socket.send(encodeAwarenessState(room.awareness));
      return;
    }
    if (message.update.byteLength > MAX_AWARENESS_UPDATE_BYTES) {
      socket.close(1009, "awareness update too large");
      return;
    }
    const expectedClientId = Number(clientId);
    if (awarenessClientIds(message.update).some((id) => id !== expectedClientId)) {
      throw new Error("An awareness update used a different client id");
    }
    const pinned = withAwarenessUser(message.update, user);
    awarenessProtocol.applyAwarenessUpdate(room.awareness, pinned, socket);
  }

  private async disconnect(specId: string, room: SpecRoom, socket: WebSocket): Promise<void> {
    room.sockets.delete(socket);
    const entry = this.rooms.get(specId);
    try {
      const clientId = room.socketClientIds.get(socket);
      room.socketClientIds.delete(socket);
      if (!clientId) return;
      const remaining = (room.clientConnectionCounts.get(clientId) ?? 1) - 1;
      if (remaining > 0) {
        room.clientConnectionCounts.set(clientId, remaining);
        return;
      }
      room.clientConnectionCounts.delete(clientId);
      awarenessProtocol.removeAwarenessStates(room.awareness, [Number(clientId)], socket);
      await this.deps.participants.disconnect(specId, clientId);
    } finally {
      if (entry) this.retireRoomIfIdle(specId, entry, room);
    }
  }

  private async applyPeerAwareness(specId: string, update: Uint8Array): Promise<void> {
    const entry = this.rooms.get(specId);
    if (!entry) return;
    const room = await entry.promise;
    if (this.rooms.get(specId) !== entry) return;
    awarenessProtocol.applyAwarenessUpdate(room.awareness, update, PEER_AWARENESS);
  }

  private async answerAwarenessQuery(specId: string): Promise<void> {
    const entry = this.rooms.get(specId);
    if (!entry) return;
    const room = await entry.promise;
    if (this.rooms.get(specId) !== entry) return;
    if (room.awareness.getStates().size === 0) return;
    const update = awarenessProtocol.encodeAwarenessUpdate(room.awareness, [
      ...room.awareness.getStates().keys(),
    ]);
    await this.deps.awarenessBus.publish(specId, update);
  }

  private broadcast(room: SpecRoom, message: Uint8Array): void {
    for (const socket of room.sockets) {
      if (socket.readyState === WebSocket.OPEN) socket.send(message);
    }
  }

  private retireRoomIfIdle(specId: string, entry: SpecRoomEntry, room: SpecRoom): void {
    if (entry.pendingUsers > 0 || room.sockets.size > 0 || room.agents.size > 0) return;
    if (this.rooms.get(specId) !== entry) return;
    this.rooms.delete(specId);
    room.unsubscribeDocument();
    room.awareness.destroy();
  }

  private runBackgroundTask(operation: string, task: Promise<void>): void {
    void task.catch((error: unknown) => {
      this.deps.onWarning?.(`${operation} failed: ${errorMessage(error)}`);
    });
  }
}

/** Build the raw UpgradeHook for `/api/v1/specs/:id/sync`. */
export function makeSpecSyncUpgradeHandler(
  deps: SpecSyncDeps,
  hub = new SpecSyncHub(deps),
): (req: IncomingMessage, socket: Socket, head: Buffer) => Promise<boolean> {
  const guard = makeSpecMemberHeaderGuard(deps.resolveMembership, deps.getSession);
  const wss = new WebSocketServer({ noServer: true });

  return async function trySpecSyncUpgrade(req, socket, head) {
    const parsed = parseSpecSyncPath(new URL(req.url ?? "/", "http://localhost"));
    if (!parsed) return false;

    const headers = requestHeaders(req);
    const authz = await guard(headers, parsed.specId);
    if (!authz.ok || !parsed.clientId) {
      const status = authz.ok ? 400 : authz.status;
      wss.handleUpgrade(req, socket, head, (ws) => {
        ws.close(4000 + status, `spec sync ${status}`);
      });
      return true;
    }

    wss.handleUpgrade(req, socket, head, (ws) => {
      void hub
        .connect(
          parsed.specId,
          parsed.clientId!,
          { id: authz.user.id, ...(authz.user.name ? { name: authz.user.name } : {}) },
          ws,
        )
        .catch(() => ws.close(1011, "spec sync failed"));
    });
    return true;
  };
}

function requestHeaders(req: IncomingMessage): Headers {
  const headers = new Headers();
  for (const key in req.headers) {
    const value = req.headers[key];
    if (value) headers.set(key, Array.isArray(value) ? value[0]! : value);
  }
  return headers;
}

function rawDataBytes(data: RawData): Uint8Array {
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (Array.isArray(data)) return Buffer.concat(data);
  return data;
}

function agentKey(input: SpecPresenceLeaveInput): string {
  return `${input.sessionId}:${input.toolCallId}`;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
