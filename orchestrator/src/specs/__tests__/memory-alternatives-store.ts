/** In-memory `spec_transcript_action` stand-in for the alternatives stage. */

import {
  stageFromRows,
  type SpecAlternativesActionRecord,
  type SpecAlternativesStore,
} from "../alternatives.ts";
import type { SpecAlternativesStage } from "@engrams/spec-document";

export class MemoryAlternativesStore implements SpecAlternativesStore {
  readonly rows = new Map<string, SpecAlternativesActionRecord>();

  async readStage(specId: string): Promise<SpecAlternativesStage | null> {
    const rows = [...this.rows.values()]
      .filter((row) => row.specId === specId)
      .sort((left, right) => right.createdAt.getTime() - left.createdAt.getTime());
    return stageFromRows(rows);
  }

  async readAction(actionId: string): Promise<SpecAlternativesActionRecord | null> {
    return this.rows.get(actionId) ?? null;
  }

  async insertAction(
    input: SpecAlternativesActionRecord,
  ): Promise<{ status: "stored" | "replayed"; action: SpecAlternativesActionRecord }> {
    const existing = this.rows.get(input.id);
    if (existing) return { status: "replayed", action: existing };
    this.rows.set(input.id, input);
    return { status: "stored", action: input };
  }
}
