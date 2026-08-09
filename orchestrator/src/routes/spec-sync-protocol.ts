import * as decoding from "lib0/decoding";
import * as encoding from "lib0/encoding";
import * as awarenessProtocol from "y-protocols/awareness";
import * as syncProtocol from "y-protocols/sync";
import type * as Y from "yjs";

export const SPEC_SYNC_MESSAGE = 0;
export const SPEC_AWARENESS_MESSAGE = 1;
export const SPEC_AWARENESS_QUERY_MESSAGE = 3;

export type SpecSyncClientMessage =
  | { kind: "sync-step-1"; stateVector: Uint8Array }
  | { kind: "sync-update"; update: Uint8Array }
  | { kind: "awareness"; update: Uint8Array }
  | { kind: "awareness-query" };

/** Decode one y-websocket compatible client message without applying it. */
export function decodeSpecSyncMessage(message: Uint8Array): SpecSyncClientMessage {
  const decoder = decoding.createDecoder(message);
  const messageType = decoding.readVarUint(decoder);
  let decoded: SpecSyncClientMessage;

  if (messageType === SPEC_SYNC_MESSAGE) {
    const syncType = decoding.readVarUint(decoder);
    if (syncType === syncProtocol.messageYjsSyncStep1) {
      decoded = { kind: "sync-step-1", stateVector: decoding.readVarUint8Array(decoder) };
    } else if (
      syncType === syncProtocol.messageYjsSyncStep2 ||
      syncType === syncProtocol.messageYjsUpdate
    ) {
      decoded = { kind: "sync-update", update: decoding.readVarUint8Array(decoder) };
    } else {
      throw new Error(`Unknown Yjs sync message type: ${syncType}`);
    }
  } else if (messageType === SPEC_AWARENESS_MESSAGE) {
    decoded = { kind: "awareness", update: decoding.readVarUint8Array(decoder) };
  } else if (messageType === SPEC_AWARENESS_QUERY_MESSAGE) {
    decoded = { kind: "awareness-query" };
  } else {
    throw new Error(`Unknown spec WebSocket message type: ${messageType}`);
  }

  if (decoder.pos !== decoder.arr.length) {
    throw new Error("The spec WebSocket message has trailing bytes");
  }
  return decoded;
}

/** Start the bidirectional Yjs synchronization handshake. */
export function encodeSyncStep1(doc: Y.Doc): Uint8Array {
  return encodeMessage(SPEC_SYNC_MESSAGE, (encoder) => {
    syncProtocol.writeSyncStep1(encoder, doc);
  });
}

/** Reply to a synchronization state vector with the missing update. */
export function encodeSyncStep2(doc: Y.Doc, stateVector: Uint8Array): Uint8Array {
  return encodeMessage(SPEC_SYNC_MESSAGE, (encoder) => {
    syncProtocol.writeSyncStep2(encoder, doc, stateVector);
  });
}

/** Wrap one durable Yjs update for WebSocket clients. */
export function encodeSyncUpdate(update: Uint8Array): Uint8Array {
  return encodeMessage(SPEC_SYNC_MESSAGE, (encoder) => {
    syncProtocol.writeUpdate(encoder, update);
  });
}

/** Wrap one ephemeral awareness update for WebSocket clients. */
export function encodeAwarenessMessage(update: Uint8Array): Uint8Array {
  return encodeMessage(SPEC_AWARENESS_MESSAGE, (encoder) => {
    encoding.writeVarUint8Array(encoder, update);
  });
}

/** Wrap the current awareness state for one client. */
export function encodeAwarenessState(
  awareness: awarenessProtocol.Awareness,
  clientIds = [...awareness.getStates().keys()],
): Uint8Array {
  return encodeAwarenessMessage(awarenessProtocol.encodeAwarenessUpdate(awareness, clientIds));
}

/** Read the Yjs client ids from an awareness update and validate its JSON. */
export function awarenessClientIds(update: Uint8Array): number[] {
  const decoder = decoding.createDecoder(update);
  const count = decoding.readVarUint(decoder);
  const clientIds: number[] = [];
  for (let index = 0; index < count; index += 1) {
    clientIds.push(decoding.readVarUint(decoder));
    decoding.readVarUint(decoder);
    JSON.parse(decoding.readVarString(decoder));
  }
  if (decoder.pos !== decoder.arr.length) {
    throw new Error("The awareness update has trailing bytes");
  }
  return clientIds;
}

/** Pin a browser awareness state to the authenticated user identity. */
export function withAwarenessUser(
  update: Uint8Array,
  user: { id: string; name?: string; color?: string },
): Uint8Array {
  return awarenessProtocol.modifyAwarenessUpdate(update, (state) => {
    if (!isRecord(state)) return state;
    const claimedUser = isRecord(state.user) ? state.user : {};
    return {
      ...state,
      user: {
        ...claimedUser,
        id: user.id,
        ...(user.name ? { name: user.name } : {}),
        ...(user.color ? { color: user.color } : {}),
      },
    };
  });
}

function encodeMessage(
  messageType: number,
  writeBody: (encoder: encoding.Encoder) => void,
): Uint8Array {
  const encoder = encoding.createEncoder();
  encoding.writeVarUint(encoder, messageType);
  writeBody(encoder);
  return encoding.toUint8Array(encoder);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
