import { describe, expect, test } from "bun:test";
import * as decoding from "lib0/decoding";
import * as encoding from "lib0/encoding";
import * as awarenessProtocol from "y-protocols/awareness";
import * as syncProtocol from "y-protocols/sync";
import * as Y from "yjs";

import {
  SPEC_AWARENESS_MESSAGE,
  SPEC_SYNC_MESSAGE,
  awarenessClientIds,
  decodeSpecSyncMessage,
  encodeAwarenessMessage,
  encodeSyncStep1,
  encodeSyncStep2,
  encodeSyncUpdate,
  withAwarenessUser,
} from "../routes/spec-sync-protocol.ts";

describe("the spec sync protocol codec", () => {
  test("round-trips the sync handshake and update messages", () => {
    const source = new Y.Doc();
    source.getText("content").insert(0, "hello");
    const target = new Y.Doc();

    const step1 = decodeSpecSyncMessage(encodeSyncStep1(target));
    expect(step1.kind).toBe("sync-step-1");
    if (step1.kind !== "sync-step-1") throw new Error("expected sync step 1");

    const step2 = decodeSpecSyncMessage(encodeSyncStep2(source, step1.stateVector));
    expect(step2.kind).toBe("sync-update");
    if (step2.kind !== "sync-update") throw new Error("expected sync update");
    Y.applyUpdate(target, step2.update);
    expect(target.getText("content").toString()).toBe("hello");

    const update = Y.encodeStateAsUpdate(source);
    expect(decodeSpecSyncMessage(encodeSyncUpdate(update))).toEqual({
      kind: "sync-update",
      update,
    });
  });

  test("pins awareness to the authenticated user", () => {
    const doc = new Y.Doc();
    const awareness = new awarenessProtocol.Awareness(doc);
    awareness.setLocalState({
      user: { id: "claimed-user", name: "Claimed Name", color: "#111111" },
      cursor: { anchor: 2, head: 4 },
      agentPresence: [
        {
          name: "engram",
          sessionId: "forged-session",
          toolCallId: "forged-tool",
          sectionId: "failure-modes",
        },
      ],
    });
    const update = awarenessProtocol.encodeAwarenessUpdate(awareness, [doc.clientID]);
    const pinned = withAwarenessUser(update, {
      id: "authenticated-user",
      name: "Actual Name",
      color: "#abcdef",
    });

    const receiver = new awarenessProtocol.Awareness(new Y.Doc());
    awarenessProtocol.applyAwarenessUpdate(receiver, pinned, "test");

    expect(awarenessClientIds(pinned)).toEqual([doc.clientID]);
    expect(receiver.getStates().get(doc.clientID)).toEqual({
      user: { id: "authenticated-user", name: "Actual Name", color: "#abcdef" },
      cursor: { anchor: 2, head: 4 },
    });
    awareness.destroy();
    receiver.destroy();
  });

  test("rejects unknown frames and trailing bytes", () => {
    const unknown = encoding.createEncoder();
    encoding.writeVarUint(unknown, 99);
    expect(() => decodeSpecSyncMessage(encoding.toUint8Array(unknown))).toThrow(
      "Unknown spec WebSocket message type",
    );

    const valid = encodeAwarenessMessage(new Uint8Array([0]));
    const trailing = new Uint8Array([...valid, 1]);
    expect(() => decodeSpecSyncMessage(trailing)).toThrow("trailing bytes");
  });

  test("uses the y-websocket outer message numbers", () => {
    const doc = new Y.Doc();
    const syncDecoder = decoding.createDecoder(encodeSyncStep1(doc));
    expect(decoding.readVarUint(syncDecoder)).toBe(SPEC_SYNC_MESSAGE);
    expect(decoding.readVarUint(syncDecoder)).toBe(syncProtocol.messageYjsSyncStep1);

    const awarenessDecoder = decoding.createDecoder(encodeAwarenessMessage(new Uint8Array([0])));
    expect(decoding.readVarUint(awarenessDecoder)).toBe(SPEC_AWARENESS_MESSAGE);
    doc.destroy();
  });
});
