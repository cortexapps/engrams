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
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const PEER_AWARENESS = Symbol("peer-awareness");
const MAX_AWARENESS_UPDATE_BYTES = 5_800;
const DEFAULT_HEARTBEAT_INTERVAL_MS = 20_000;
const SUPERSEDED_CLOSE_CODE = 4009;

export interface SpecDocumentUpdate {
  update: Uint8Array;
}

export interface SpecSyncDocuments {
  loadDoc(specId: string): Promise<{ doc: Y.Doc }>;
  applyUpdate(specId: string, update: Uint8Array, clientId: string): Promise<unknown>;
  subscribe(specId: string, listener: (event: SpecDocumentUpdate) => void): () => void;
  evict(specId: string): void;
}

export interface SpecParticipantStore {
  connect(specId: string, clientId: string, userId: string): Promise<bigint>;
  renew(specId: string, clientId: string, epoch: bigint): Promise<boolean>;
  disconnect(specId: string, clientId: string, epoch: bigint): Promise<void>;
}

export interface SpecAwarenessBus {
  start(handlers: {
    update(specId: string, update: Uint8Array): void;
    query(specId: string): void;
    participantConnected(specId: string, clientId: string, epoch: bigint): void;
    reconnect?(): void;
  }): Promise<() => Promise<void>>;
  publish(specId: string, update: Uint8Array): Promise<void>;
  query(specId: string): Promise<void>;
  publishParticipantConnected(specId: string, clientId: string, epoch: bigint): Promise<void>;
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
  heartbeatIntervalMs?: number;
  timers?: SpecSyncTimers;
}

export interface SpecSyncTimers {
  setInterval(callback: () => void, milliseconds: number): unknown;
  clearInterval(handle: unknown): void;
}

export interface SpecSyncSocket {
  readonly readyState: number;
  on(event: "message", listener: (data: RawData, isBinary: boolean) => void): this;
  on(event: "pong", listener: () => void): this;
  on(event: "error", listener: (error: Error) => void): this;
  once(event: "close", listener: () => void): this;
  off(event: "pong", listener: () => void): this;
  off(event: "error", listener: (error: Error) => void): this;
  send(data: Uint8Array): void;
  close(code?: number, reason?: string): void;
  ping(): void;
  terminate(): void;
}

export interface ParsedSpecSyncPath {
  specId: string | null;
  clientId: string | null;
}

export function parseSpecSyncPath(url: URL): ParsedSpecSyncPath | null {
  const match = SPEC_SYNC_PATH.exec(url.pathname);
  if (!match) return null;
  let specId: string;
  try {
    specId = decodeURIComponent(match[1]!);
  } catch {
    return { specId: null, clientId: null };
  }
  const rawClientId = url.searchParams.get("clientId");
  if (!UUID.test(specId) || !rawClientId || !/^\d+$/.test(rawClientId)) {
    return { specId: UUID.test(specId) ? specId : null, clientId: null };
  }
  const clientId = Number(rawClientId);
  if (!Number.isSafeInteger(clientId) || clientId < 0 || clientId > 0xffff_ffff) {
    return { specId, clientId: null };
  }
  return { specId, clientId: String(clientId) };
}

interface AgentPresenceState extends SpecPresenceEnterInput {}

interface SpecRoom {
  doc: Y.Doc;
  awareness: awarenessProtocol.Awareness;
  sockets: Set<SpecSyncSocket>;
  socketClientIds: Map<SpecSyncSocket, string>;
  socketParticipantEpochs: Map<SpecSyncSocket, bigint>;
  clientSockets: Map<string, SpecSyncSocket>;
  socketHeartbeatStops: Map<SpecSyncSocket, () => void>;
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
  private readonly timers: SpecSyncTimers;
  private readonly heartbeatIntervalMs: number;

  constructor(
    private readonly deps: Pick<
      SpecSyncDeps,
      "documents" | "participants" | "awarenessBus" | "onWarning" | "heartbeatIntervalMs" | "timers"
    >,
  ) {
    this.timers = deps.timers ?? systemTimers;
    this.heartbeatIntervalMs = deps.heartbeatIntervalMs ?? DEFAULT_HEARTBEAT_INTERVAL_MS;
  }

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
      participantConnected: (specId, clientId, epoch) => {
        this.runBackgroundTask(
          `Supersede participant connection for spec ${specId}`,
          this.supersedeParticipantConnection(specId, clientId, epoch),
        );
      },
      reconnect: () => {
        for (const specId of this.rooms.keys()) {
          this.runBackgroundTask(
            `Query peer awareness for spec ${specId}`,
            this.deps.awarenessBus.query(specId),
          );
        }
      },
    });
  }

  async stop(): Promise<void> {
    const stopBus = this.stopBus;
    this.stopBus = null;
    if (stopBus) await stopBus();
    for (const [specId, entry] of this.rooms) {
      const room = await entry.promise;
      for (const socket of room.sockets) {
        room.socketHeartbeatStops.get(socket)?.();
        socket.close(1001, "orchestrator stopping");
      }
      room.unsubscribeDocument();
      room.awareness.destroy();
      this.deps.documents.evict(specId);
    }
    this.rooms.clear();
  }

  async connect(
    specId: string,
    clientId: string,
    user: { id: string; name?: string },
    socket: SpecSyncSocket,
  ): Promise<void> {
    const entry = this.acquireRoom(specId);
    let room: SpecRoom | undefined;
    let closed = false;
    let joined = false;
    const onError = (error: Error) => {
      this.deps.onWarning?.(`Spec sync socket failed for spec ${specId}: ${error.message}`);
      if (socket.readyState !== WebSocket.CLOSED) socket.terminate();
    };
    socket.on("error", onError);
    socket.once("close", () => {
      closed = true;
      socket.off("error", onError);
      if (!joined || !room) return;
      this.runBackgroundTask(
        `Disconnect participant from spec ${specId}`,
        this.disconnect(specId, room, socket),
      );
    });
    try {
      room = await entry.promise;
      if (closed) return;
      const participantEpoch = await this.deps.participants.connect(specId, clientId, user.id);
      if (closed) {
        try {
          await this.deps.participants.disconnect(specId, clientId, participantEpoch);
        } catch (error: unknown) {
          this.deps.onWarning?.(
            `Disconnect participant from spec ${specId} failed: ${errorMessage(error)}`,
          );
        }
        return;
      }
      const currentSocket = room.clientSockets.get(clientId);
      if (currentSocket) {
        const currentEpoch = room.socketParticipantEpochs.get(currentSocket);
        if (currentEpoch === undefined) {
          throw new Error(`Spec client ${clientId} has no participant epoch`);
        }
        if (currentEpoch >= participantEpoch) {
          this.runBackgroundTask(
            `Disconnect stale participant from spec ${specId}`,
            this.deps.participants.disconnect(specId, clientId, participantEpoch),
          );
          socket.close(SUPERSEDED_CLOSE_CODE, "superseded by a newer connection");
          return;
        }
        const superseded = this.detachSocket(room, currentSocket, false);
        this.runBackgroundTask(
          `Supersede participant connection for spec ${specId}`,
          this.finishSupersession(specId, entry, room, superseded),
        );
        currentSocket.close(SUPERSEDED_CLOSE_CODE, "superseded by a newer connection");
      }
      room.sockets.add(socket);
      room.socketClientIds.set(socket, clientId);
      room.socketParticipantEpochs.set(socket, participantEpoch);
      room.clientSockets.set(clientId, socket);
      this.startHeartbeat(specId, room, socket, clientId, participantEpoch);
      joined = true;
      this.runBackgroundTask(
        `Publish participant connection for spec ${specId}`,
        this.deps.awarenessBus.publishParticipantConnected(specId, clientId, participantEpoch),
      );
    } finally {
      entry.pendingUsers -= 1;
      if (room) this.retireRoomIfIdle(specId, entry, room);
    }

    socket.on("message", (data, isBinary) => {
      if (room.clientSockets.get(clientId) !== socket) return;
      if (!isBinary) {
        socket.close(1003, "binary messages required");
        return;
      }
      void this.receive(specId, room, socket, clientId, user, rawDataBytes(data)).catch(() => {
        socket.close(1003, "invalid spec sync message");
      });
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
      socketParticipantEpochs: new Map(),
      clientSockets: new Map(),
      socketHeartbeatStops: new Map(),
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
    socket: SpecSyncSocket,
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

  private async disconnect(specId: string, room: SpecRoom, socket: SpecSyncSocket): Promise<void> {
    const participant = this.detachSocket(room, socket, true);
    if (!participant) return;
    const entry = this.rooms.get(specId);
    try {
      await this.deps.participants.disconnect(specId, participant.clientId, participant.epoch);
    } finally {
      if (entry) this.retireRoomIfIdle(specId, entry, room);
    }
  }

  private detachSocket(
    room: SpecRoom,
    socket: SpecSyncSocket,
    removeCurrentAwareness: boolean,
  ): { clientId: string; epoch: bigint; awarenessUpdate?: Uint8Array } | null {
    room.socketHeartbeatStops.get(socket)?.();
    room.sockets.delete(socket);
    const clientId = room.socketClientIds.get(socket);
    const epoch = room.socketParticipantEpochs.get(socket);
    room.socketClientIds.delete(socket);
    room.socketParticipantEpochs.delete(socket);
    if (clientId === undefined || epoch === undefined) return null;

    const isCurrentSocket = room.clientSockets.get(clientId) === socket;
    if (!isCurrentSocket) return { clientId, epoch };
    room.clientSockets.delete(clientId);
    if (removeCurrentAwareness) {
      awarenessProtocol.removeAwarenessStates(room.awareness, [Number(clientId)], socket);
      return { clientId, epoch };
    }

    const awarenessUpdate = room.awareness.getStates().has(Number(clientId))
      ? awarenessProtocol.encodeAwarenessUpdate(room.awareness, [Number(clientId)])
      : undefined;
    return { clientId, epoch, awarenessUpdate };
  }

  private async finishSupersession(
    specId: string,
    entry: SpecRoomEntry,
    room: SpecRoom,
    participant: { clientId: string; epoch: bigint; awarenessUpdate?: Uint8Array } | null,
  ): Promise<void> {
    if (!participant) return;
    try {
      if (participant.awarenessUpdate) {
        await this.deps.awarenessBus.publish(specId, participant.awarenessUpdate);
      }
    } finally {
      try {
        await this.deps.participants.disconnect(specId, participant.clientId, participant.epoch);
      } finally {
        this.retireRoomIfIdle(specId, entry, room);
      }
    }
  }

  private async applyPeerAwareness(specId: string, update: Uint8Array): Promise<void> {
    const entry = this.rooms.get(specId);
    if (!entry) return;
    const room = await entry.promise;
    if (this.rooms.get(specId) !== entry) return;
    awarenessProtocol.applyAwarenessUpdate(room.awareness, update, PEER_AWARENESS);
  }

  private async supersedeParticipantConnection(
    specId: string,
    clientId: string,
    epoch: bigint,
  ): Promise<void> {
    const entry = this.rooms.get(specId);
    if (!entry) return;
    const room = await entry.promise;
    if (this.rooms.get(specId) !== entry) return;
    const socket = room.clientSockets.get(clientId);
    if (!socket) return;
    const socketEpoch = room.socketParticipantEpochs.get(socket);
    if (socketEpoch === undefined || socketEpoch >= epoch) return;
    const superseded = this.detachSocket(room, socket, false);
    this.runBackgroundTask(
      `Supersede participant connection for spec ${specId}`,
      this.finishSupersession(specId, entry, room, superseded),
    );
    socket.close(SUPERSEDED_CLOSE_CODE, "superseded by a newer connection");
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

  private startHeartbeat(
    specId: string,
    room: SpecRoom,
    socket: SpecSyncSocket,
    clientId: string,
    participantEpoch: bigint,
  ): void {
    let awaitingPong = false;
    const onPong = () => {
      awaitingPong = false;
      this.runBackgroundTask(
        `Renew participant lease for spec ${specId}`,
        this.renewParticipant(specId, room, socket, clientId, participantEpoch),
      );
    };
    socket.on("pong", onPong);
    const timer = this.timers.setInterval(() => {
      if (socket.readyState !== WebSocket.OPEN) return;
      if (awaitingPong) {
        socket.terminate();
        return;
      }
      awaitingPong = true;
      try {
        socket.ping();
      } catch (error: unknown) {
        this.deps.onWarning?.(
          `Spec sync heartbeat failed for spec ${specId}: ${error instanceof Error ? error.message : String(error)}`,
        );
        socket.terminate();
      }
    }, this.heartbeatIntervalMs);
    room.socketHeartbeatStops.set(socket, () => {
      if (!room.socketHeartbeatStops.delete(socket)) return;
      this.timers.clearInterval(timer);
      socket.off("pong", onPong);
    });
  }

  private async renewParticipant(
    specId: string,
    room: SpecRoom,
    socket: SpecSyncSocket,
    clientId: string,
    participantEpoch: bigint,
  ): Promise<void> {
    const renewed = await this.deps.participants.renew(specId, clientId, participantEpoch);
    if (!renewed && room.clientSockets.get(clientId) === socket) socket.terminate();
  }

  private retireRoomIfIdle(specId: string, entry: SpecRoomEntry, room: SpecRoom): void {
    if (entry.pendingUsers > 0 || room.sockets.size > 0 || room.agents.size > 0) return;
    if (this.rooms.get(specId) !== entry) return;
    this.rooms.delete(specId);
    room.unsubscribeDocument();
    room.awareness.destroy();
    this.deps.documents.evict(specId);
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
    let parsed: ParsedSpecSyncPath | null;
    try {
      parsed = parseSpecSyncPath(new URL(req.url ?? "/", "http://localhost"));
    } catch {
      return rejectUpgrade(wss, req, socket, head, 400);
    }
    if (!parsed) return false;

    try {
      if (!parsed.specId || !parsed.clientId) {
        return rejectUpgrade(wss, req, socket, head, 400);
      }
      const headers = requestHeaders(req);
      const authz = await guard(headers, parsed.specId);
      if (!authz.ok) return rejectUpgrade(wss, req, socket, head, authz.status);
      const specId = parsed.specId;
      const clientId = parsed.clientId;

      wss.handleUpgrade(req, socket, head, (ws) => {
        void hub
          .connect(
            specId,
            clientId,
            { id: authz.user.id, ...(authz.user.name ? { name: authz.user.name } : {}) },
            ws,
          )
          .catch(() => ws.close(1011, "spec sync failed"));
      });
      return true;
    } catch (error: unknown) {
      deps.onWarning?.(`Spec sync upgrade failed: ${errorMessage(error)}`);
      return rejectUpgrade(wss, req, socket, head, 500);
    }
  };
}

function rejectUpgrade(
  wss: WebSocketServer,
  req: IncomingMessage,
  socket: Socket,
  head: Buffer,
  status: number,
): true {
  try {
    wss.handleUpgrade(req, socket, head, (ws) => {
      ws.close(Math.min(4000 + status, 4999), `spec sync ${status}`);
    });
  } catch {
    socket.destroy();
  }
  return true;
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

const systemTimers: SpecSyncTimers = {
  setInterval: (callback, milliseconds) => setInterval(callback, milliseconds),
  // The default setter creates this exact timer handle type.
  clearInterval: (handle) => clearInterval(handle as ReturnType<typeof setInterval>),
};
