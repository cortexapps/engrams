/** In-memory `SpecDocumentStore` for the tool and alternatives suites. */

import type {
  CompactSnapshotInput,
  SpecDocumentCheckpoint,
  SpecDocumentStore,
  SpecSnapshotRecord,
  SpecTrackedEditActionInput,
  SpecTrackedEditActionRecord,
  SpecUpdateEffects,
  SpecUpdateInsertResult,
  SpecUpdateRecord,
} from "../doc-service.ts";

export class MemoryDocumentStore implements SpecDocumentStore {
  seq = 0n;
  semanticSeq = 0n;
  readonly updates: SpecUpdateRecord[] = [];
  readonly clientIds: Array<string | null> = [];
  readonly actions = new Map<string, SpecTrackedEditActionRecord>();
  lastEffects: SpecUpdateEffects | null = null;
  lastCheckpoint: SpecDocumentCheckpoint | null = null;

  async readSnapshot(): Promise<SpecSnapshotRecord | null> {
    return null;
  }

  async readUpdatesAfter(_specId: string, afterSeq: bigint): Promise<SpecUpdateRecord[]> {
    return this.updates.filter((row) => row.seq > afterSeq);
  }

  async insertUpdateIfLatest(
    _specId: string,
    expectedSeq: bigint,
    update: Uint8Array,
    clientId: string | null,
    effects: SpecUpdateEffects,
    _participantEpoch?: bigint,
    transcriptAction?: SpecTrackedEditActionInput & { createdAt: Date },
  ): Promise<SpecUpdateInsertResult | null> {
    if (expectedSeq !== this.seq) return null;
    this.seq += 1n;
    if (effects.semanticChanged) this.semanticSeq += 1n;
    this.lastEffects = effects;
    this.clientIds.push(clientId);
    this.updates.push({
      seq: this.seq,
      semanticDocSeq: this.semanticSeq,
      update: update.slice(),
      clientId,
    });
    if (transcriptAction) {
      const action: SpecTrackedEditActionRecord = {
        ...transcriptAction,
        result: {
          applied: true,
          newRev: this.semanticSeq,
          concurrentEditors: transcriptAction.concurrentEditors,
          transcriptChip: transcriptAction.chip,
        },
        deliveredAt: null,
      };
      this.actions.set(action.id, action);
    }
    return { seq: this.seq, semanticDocSeq: this.semanticSeq };
  }

  async readTrackedEditAction(actionId: string): Promise<SpecTrackedEditActionRecord | null> {
    return this.actions.get(actionId) ?? null;
  }

  async insertCheckpointAndUpdateIfLatest(
    specId: string,
    expectedSeq: bigint,
    checkpoint: SpecDocumentCheckpoint,
    update: Uint8Array,
    clientId: string | null,
    effects: SpecUpdateEffects,
  ): Promise<SpecUpdateInsertResult | null> {
    const inserted = await this.insertUpdateIfLatest(specId, expectedSeq, update, clientId, effects);
    if (inserted) this.lastCheckpoint = checkpoint;
    return inserted;
  }

  async notifyUpdate(): Promise<void> {}

  async compactSnapshot(_input: CompactSnapshotInput): Promise<boolean> {
    return false;
  }

  async listen(): Promise<() => Promise<void>> {
    return async () => {};
  }
}
