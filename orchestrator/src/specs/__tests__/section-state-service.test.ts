import { describe, expect, test } from "bun:test";

import {
  SectionStateConflictError,
  SectionStateService,
  SectionStateTranscriptDrainer,
  type PersistSectionStateActionInput,
  type PersistSectionStateActionResult,
  type SectionStateStore,
  type SectionStateTranscriptAction,
  type SectionStateTranscriptPublisher,
} from "../section-state-service.ts";
import type {
  SectionStateContext,
  SectionStateTranscriptChip,
  SectionStateValue,
} from "../section-state.ts";

const context: SectionStateContext = {
  specId: "00000000-0000-4000-8000-000000000001",
  sectionId: "failure-modes",
  sectionTitle: "Failure modes",
  allowsNa: true,
};

class MemorySectionStateStore implements SectionStateStore {
  value: SectionStateValue = { state: "empty", naReason: null };
  readonly actions = new Map<string, SectionStateTranscriptAction>();
  rejectWrite = false;
  rejectDeliveryMarkOnce = false;
  stateWrites = 0;
  docSeq = 1n;

  async read(): Promise<SectionStateValue> {
    return this.value;
  }

  async readAction(actionId: string): Promise<SectionStateTranscriptAction | null> {
    return this.actions.get(actionId) ?? null;
  }

  async persistStateAction(
    input: PersistSectionStateActionInput,
  ): Promise<PersistSectionStateActionResult> {
    const existing = this.actions.get(input.actionId);
    if (existing) return { status: "replayed", action: existing };
    if (
      this.rejectWrite ||
      (input.expectedDocSeq !== undefined && input.expectedDocSeq !== this.docSeq) ||
      this.value.state !== input.expected.state ||
      this.value.naReason !== input.expected.naReason
    ) {
      return { status: "conflict" };
    }
    this.value = input.next;
    this.stateWrites += 1;
    const action: SectionStateTranscriptAction = {
      id: input.actionId,
      specId: input.specId,
      sectionId: input.sectionId,
      requestFingerprint: input.requestFingerprint,
      chip: input.chip,
      createdAt: input.at,
      deliveredAt: null,
    };
    this.actions.set(action.id, action);
    return { status: "stored", action };
  }

  async listPendingActions(limit: number): Promise<SectionStateTranscriptAction[]> {
    return [...this.actions.values()]
      .filter((action) => action.deliveredAt == null)
      .slice(0, limit);
  }

  async markActionDelivered(actionId: string, deliveredAt: Date): Promise<boolean> {
    if (this.rejectDeliveryMarkOnce) {
      this.rejectDeliveryMarkOnce = false;
      return false;
    }
    const action = this.actions.get(actionId);
    if (!action || action.deliveredAt) return false;
    action.deliveredAt = deliveredAt;
    return true;
  }
}

class MemoryTranscriptPublisher implements SectionStateTranscriptPublisher {
  readonly publications: Array<{ actionId: string; chip: SectionStateTranscriptChip }> = [];
  readonly attempts: string[] = [];
  private readonly deliveredActionIds = new Set<string>();
  fail = false;

  async publish(actionId: string, chip: SectionStateTranscriptChip): Promise<void> {
    this.attempts.push(actionId);
    if (this.fail) throw new Error("The transcript is unavailable.");
    if (this.deliveredActionIds.has(actionId)) return;
    this.deliveredActionIds.add(actionId);
    this.publications.push({ actionId, chip });
  }
}

function serviceWith(store: MemorySectionStateStore, transcript: MemoryTranscriptPublisher) {
  return new SectionStateService({
    store,
    transcript,
    now: () => new Date("2026-08-09T12:00:00.000Z"),
  });
}

describe("section state service", () => {
  test("defers transcript delivery until the drainer runs", async () => {
    const store = new MemorySectionStateStore();
    const transcript = new MemoryTranscriptPublisher();
    const service = serviceWith(store, transcript);

    const first = await service.transitionDeferred({
      actionId: "deferred-action",
      context,
      target: "drafted",
      actorUserId: "user-1",
    });
    const replay = await service.transitionDeferred({
      actionId: "deferred-action",
      context,
      target: "drafted",
      actorUserId: "user-1",
    });

    expect(replay).toEqual(first);
    expect(store.stateWrites).toBe(1);
    expect(store.actions.get("deferred-action")?.deliveredAt).toBeNull();
    expect(transcript.publications).toHaveLength(0);

    const drainer = new SectionStateTranscriptDrainer(
      store,
      transcript,
      () => new Date("2026-08-09T12:01:00.000Z"),
    );
    expect(await drainer.runOnce()).toEqual({ delivered: 1, failed: 0 });
    expect(transcript.publications).toEqual([
      { actionId: "deferred-action", chip: first.transcriptChip },
    ]);
    expect(store.actions.get("deferred-action")?.deliveredAt).toEqual(
      new Date("2026-08-09T12:01:00.000Z"),
    );
  });

  test("publishes one durable chip after every persisted transition", async () => {
    const store = new MemorySectionStateStore();
    const transcript = new MemoryTranscriptPublisher();
    const service = serviceWith(store, transcript);

    const drafted = await service.transition({
      actionId: "draft-action",
      context,
      target: "drafted",
      actorUserId: "user-1",
    });
    const confirmed = await service.transition({
      actionId: "confirm-action",
      context,
      target: "confirmed",
      actorUserId: "user-1",
    });

    expect(store.value).toEqual({ state: "confirmed", naReason: null });
    expect(store.stateWrites).toBe(2);
    expect(transcript.publications).toEqual([
      { actionId: "draft-action", chip: drafted.transcriptChip },
      { actionId: "confirm-action", chip: confirmed.transcriptChip },
    ]);
  });

  test("publishes one chip for a human edit and one chip for its undo", async () => {
    const store = new MemorySectionStateStore();
    const transcript = new MemoryTranscriptPublisher();
    const service = serviceWith(store, transcript);

    const edit = await service.recordHumanEdit({
      actionId: "edit-action",
      context,
      actorUserId: "user-1",
    });
    if (!edit) throw new Error("The empty section edit must change its state.");
    const unchanged = await service.recordHumanEdit({
      actionId: "second-edit-action",
      context,
      actorUserId: "user-1",
    });
    const undo = await service.undo({
      actionId: "undo-action",
      context,
      undo: edit.transcriptChip.undo,
      actorUserId: "user-1",
    });

    expect(unchanged).toBeNull();
    expect(transcript.publications.map(({ actionId }) => actionId)).toEqual([
      "edit-action",
      "undo-action",
    ]);
    expect(transcript.publications[1]!.chip).toEqual(undo.transcriptChip);
  });

  test("retries a failed publish without a second state write", async () => {
    const store = new MemorySectionStateStore();
    const transcript = new MemoryTranscriptPublisher();
    transcript.fail = true;
    const service = serviceWith(store, transcript);
    const input = {
      actionId: "retry-action",
      context,
      target: "drafted" as const,
      actorUserId: "user-1",
    };

    await expect(service.transition(input)).rejects.toThrow("transcript is unavailable");
    expect(store.value.state).toBe("drafted");
    expect(store.stateWrites).toBe(1);
    expect(await store.listPendingActions(10)).toHaveLength(1);

    transcript.fail = false;
    const replay = await service.transition(input);
    expect(replay.value.state).toBe("drafted");
    expect(store.stateWrites).toBe(1);
    expect(transcript.publications.map(({ actionId }) => actionId)).toEqual(["retry-action"]);
    expect(await store.listPendingActions(10)).toHaveLength(0);
  });

  test("the drainer retains a failed action and delivers it on a later pass", async () => {
    const store = new MemorySectionStateStore();
    const transcript = new MemoryTranscriptPublisher();
    transcript.fail = true;
    const service = serviceWith(store, transcript);
    await expect(
      service.transition({
        actionId: "drain-action",
        context,
        target: "drafted",
        actorUserId: "user-1",
      }),
    ).rejects.toThrow();
    const drainer = new SectionStateTranscriptDrainer(
      store,
      transcript,
      () => new Date("2026-08-09T12:01:00.000Z"),
    );

    expect(await drainer.runOnce()).toEqual({ delivered: 0, failed: 1 });
    transcript.fail = false;
    expect(await drainer.runOnce()).toEqual({ delivered: 1, failed: 0 });
    expect(await drainer.runOnce()).toEqual({ delivered: 0, failed: 0 });
  });

  test("the stable delivery key prevents a duplicate after a mark failure", async () => {
    const store = new MemorySectionStateStore();
    store.rejectDeliveryMarkOnce = true;
    const transcript = new MemoryTranscriptPublisher();
    const service = serviceWith(store, transcript);
    const input = {
      actionId: "mark-retry-action",
      context,
      target: "drafted" as const,
      actorUserId: "user-1",
    };

    await service.transition(input);
    expect(await store.listPendingActions(10)).toHaveLength(1);
    await service.transition(input);

    expect(transcript.attempts).toEqual(["mark-retry-action", "mark-retry-action"]);
    expect(transcript.publications).toHaveLength(1);
    expect(store.stateWrites).toBe(1);
    expect(await store.listPendingActions(10)).toHaveLength(0);
  });

  test("rejects one action ID for a different command", async () => {
    const store = new MemorySectionStateStore();
    const transcript = new MemoryTranscriptPublisher();
    const service = serviceWith(store, transcript);
    await service.transition({
      actionId: "reused-action",
      context,
      target: "drafted",
      actorUserId: "user-1",
    });

    await expect(
      service.transition({
        actionId: "reused-action",
        context,
        target: "confirmed",
        actorUserId: "user-1",
      }),
    ).rejects.toThrow("different command");
    expect(store.stateWrites).toBe(1);
    expect(transcript.publications).toHaveLength(1);
  });

  test("replays a command when context keys use a different construction order", async () => {
    const store = new MemorySectionStateStore();
    const transcript = new MemoryTranscriptPublisher();
    const service = serviceWith(store, transcript);
    await service.transition({
      actionId: "ordered-action",
      context,
      target: "drafted",
      actorUserId: "user-1",
    });
    const reorderedContext: SectionStateContext = {
      allowsNa: true,
      sectionTitle: "Failure modes",
      sectionId: context.sectionId,
      specId: context.specId,
    };

    const replay = await service.transition({
      actionId: "ordered-action",
      context: reorderedContext,
      target: "drafted",
      actorUserId: "user-1",
    });

    expect(replay.value.state).toBe("drafted");
    expect(store.stateWrites).toBe(1);
  });

  test("does not create an action for a stale state write", async () => {
    const store = new MemorySectionStateStore();
    store.rejectWrite = true;
    const transcript = new MemoryTranscriptPublisher();
    const service = serviceWith(store, transcript);

    await expect(
      service.transition({
        actionId: "stale-action",
        context,
        target: "drafted",
        actorUserId: "user-1",
      }),
    ).rejects.toBeInstanceOf(SectionStateConflictError);
    expect(store.actions.size).toBe(0);
    expect(transcript.publications).toHaveLength(0);
  });

  test("does not change state at a stale expected document revision", async () => {
    const store = new MemorySectionStateStore();
    const transcript = new MemoryTranscriptPublisher();
    const service = serviceWith(store, transcript);

    await expect(
      service.transition({
        actionId: "stale-document-action",
        context,
        target: "drafted",
        actorUserId: "user-1",
        expectedDocSeq: 0n,
      }),
    ).rejects.toBeInstanceOf(SectionStateConflictError);
    expect(store.value.state).toBe("empty");
    expect(store.actions.size).toBe(0);
  });
});
