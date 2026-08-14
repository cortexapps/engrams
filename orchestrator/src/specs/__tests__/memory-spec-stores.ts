/** In-memory section-state and open-question stores for the spec suites. */

import type {
  CreateOpenQuestionInput,
  OpenQuestionRecord,
  OpenQuestionStore,
  ResolveOpenQuestionInput,
} from "../open-questions.ts";
import type {
  PersistSectionStateActionInput,
  PersistSectionStateActionResult,
  SectionStateStore,
  SectionStateTranscriptAction,
} from "../section-state-service.ts";
import type { SectionStateValue } from "../section-state.ts";
import type {
  CreateSpecChatMessageInput,
  SpecChatMessageRecord,
  SpecMessageCursor,
  SpecMessageStore,
} from "../../routes/spec-messages.ts";

export class MemorySpecMessageStore implements SpecMessageStore {
  sessionId: string | null = null;
  readonly rows: SpecChatMessageRecord[] = [];
  now = () => new Date();

  async resolveSessionId(): Promise<string | null> {
    return this.sessionId;
  }

  async insertMessage(input: CreateSpecChatMessageInput): Promise<void> {
    this.rows.push({ ...input, createdAt: this.now().toISOString() });
  }

  async listMessages(
    specId: string,
    after: SpecMessageCursor | undefined,
    limit: number,
  ): Promise<SpecChatMessageRecord[]> {
    const cursorRow = after
      ? this.rows.find(
          (row) =>
            row.specId === specId &&
            row.promptId === after.promptId &&
            new Date(row.createdAt).getTime() === new Date(after.createdAt).getTime(),
        )
      : undefined;
    return this.rows
      .filter(
        (row) =>
          row.specId === specId &&
          (!after ||
            (cursorRow !== undefined &&
              (row.createdAt > cursorRow.createdAt ||
                (row.createdAt === cursorRow.createdAt && row.promptId > cursorRow.promptId)))),
      )
      .sort(
        (left, right) =>
          left.createdAt.localeCompare(right.createdAt) ||
          left.promptId.localeCompare(right.promptId),
      )
      .slice(0, limit);
  }
}

export class MemorySectionStore implements SectionStateStore {
  readonly values = new Map<string, SectionStateValue>();
  readonly actions = new Map<string, SectionStateTranscriptAction>();
  docSeq = 1n;

  async read(_specId: string, sectionId: string): Promise<SectionStateValue> {
    return this.values.get(sectionId) ?? { state: "open", naReason: null };
  }

  async readAction(actionId: string): Promise<SectionStateTranscriptAction | null> {
    return this.actions.get(actionId) ?? null;
  }

  async persistStateAction(
    input: PersistSectionStateActionInput,
  ): Promise<PersistSectionStateActionResult> {
    const existing = this.actions.get(input.actionId);
    if (existing) return { status: "replayed", action: existing };
    const current = await this.read(input.specId, input.sectionId);
    if (
      (input.expectedDocSeq !== undefined && input.expectedDocSeq !== this.docSeq) ||
      current.state !== input.expected.state ||
      current.naReason !== input.expected.naReason
    ) {
      return { status: "conflict" };
    }
    this.values.set(input.sectionId, input.next);
    const action: SectionStateTranscriptAction = {
      id: input.actionId,
      specId: input.specId,
      sectionId: input.sectionId,
      requestFingerprint: input.requestFingerprint,
      chip: input.chip,
      actorUserId: input.actorUserId,
      createdAt: input.at,
      deliveredAt: null,
    };
    this.actions.set(action.id, action);
    return { status: "stored", action };
  }

  async listPendingActions(limit: number): Promise<SectionStateTranscriptAction[]> {
    return [...this.actions.values()].filter((action) => !action.deliveredAt).slice(0, limit);
  }

  async markActionDelivered(actionId: string, deliveredAt: Date): Promise<boolean> {
    const action = this.actions.get(actionId);
    if (!action || action.deliveredAt) return false;
    action.deliveredAt = deliveredAt;
    return true;
  }
}

export class MemoryQuestionStore implements OpenQuestionStore {
  readonly rows = new Map<string, OpenQuestionRecord>();

  async find(id: string): Promise<OpenQuestionRecord | null> {
    return this.rows.get(id) ?? null;
  }

  async countOpenBySection(): Promise<Record<string, number>> {
    return {};
  }

  async create(input: CreateOpenQuestionInput): Promise<OpenQuestionRecord> {
    const existing = this.rows.get(input.id);
    if (existing) return existing;
    const row: OpenQuestionRecord = {
      ...input,
      state: "open",
      resolutionLink: null,
      resolvedBy: null,
      resolvedAt: null,
    };
    this.rows.set(row.id, row);
    return row;
  }

  async resolve(input: ResolveOpenQuestionInput): Promise<boolean> {
    const row = this.rows.get(input.id);
    if (!row || row.state !== input.expectedState) return false;
    this.rows.set(input.id, {
      ...row,
      state: "resolved",
      resolutionLink: input.resolutionLink,
      resolvedBy: input.resolvedBy,
      resolvedAt: input.resolvedAt,
    });
    return true;
  }
}
