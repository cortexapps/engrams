/**
 * The alternatives stage (ADR 0114 D6, requirement R20).
 *
 * The cards and the decision are durable structured records. They ride the
 * existing `spec_transcript_action` ledger, keyed by a stable action ID, so
 * this stage needs no table of its own. The pick renders one canonical
 * §Alternatives considered body and writes it through the ordinary section
 * mutation.
 */

import { createHash } from "node:crypto";

import {
  findSection,
  parseMarkdownBlocks,
  renderAlternativesConsidered,
  replaceSection,
  schema,
  SPEC_ALTERNATIVES_REASON_MAX_CHARS,
  SPEC_ALTERNATIVES_SECTION_KEY,
  SpecAlternativesError,
  validateSpecAlternatives,
  type AlternativesDecidedTranscriptChip,
  type AlternativesProposedTranscriptChip,
  type SpecAlternativesProposal,
  type SpecAlternativesStage,
} from "@engrams/spec-document";
import type { Node as ProseMirrorNode } from "prosemirror-model";
import type { Pool } from "pg";

import {
  proseMirrorDocument,
  SpecDocumentRevisionConflictError,
  type SpecDocumentService,
} from "./doc-service.ts";

export interface SpecAlternativesActionRecord {
  id: string;
  specId: string;
  sectionId: string;
  requestFingerprint: string;
  chip: AlternativesProposedTranscriptChip | AlternativesDecidedTranscriptChip;
  createdAt: Date;
}

export interface SpecAlternativesStore {
  /** The newest proposal and its decision, or null when the stage never ran. */
  readStage(specId: string): Promise<SpecAlternativesStage | null>;
  readAction(actionId: string): Promise<SpecAlternativesActionRecord | null>;
  /** Insert, or return the row that a replay already stored. */
  insertAction(
    input: SpecAlternativesActionRecord,
  ): Promise<{ status: "stored" | "replayed"; action: SpecAlternativesActionRecord }>;
}

export class SpecAlternativesConflictError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SpecAlternativesConflictError";
  }
}

interface AlternativesActionRow {
  id: string;
  spec_id: string;
  section_id: string;
  request_fingerprint: string;
  chip: AlternativesProposedTranscriptChip | AlternativesDecidedTranscriptChip;
  created_at: Date;
}

const STAGE_KINDS = "('spec_alternatives_proposed','spec_alternatives_decided')";

export class PostgresSpecAlternativesStore implements SpecAlternativesStore {
  constructor(private readonly pool: Pool) {}

  async readStage(specId: string): Promise<SpecAlternativesStage | null> {
    const result = await this.pool.query<AlternativesActionRow>(
      `SELECT id, spec_id, section_id, request_fingerprint, chip, created_at
         FROM spec_transcript_action
        WHERE spec_id = $1 AND chip->>'kind' IN ${STAGE_KINDS}
        ORDER BY created_at DESC, id DESC`,
      [specId],
    );
    return stageFromRows(result.rows.map(actionValue));
  }

  async readAction(actionId: string): Promise<SpecAlternativesActionRecord | null> {
    const result = await this.pool.query<AlternativesActionRow>(
      `SELECT id, spec_id, section_id, request_fingerprint, chip, created_at
         FROM spec_transcript_action
        WHERE id = $1 AND chip->>'kind' IN ${STAGE_KINDS}`,
      [actionId],
    );
    const row = result.rows[0];
    return row ? actionValue(row) : null;
  }

  async insertAction(
    input: SpecAlternativesActionRecord,
  ): Promise<{ status: "stored" | "replayed"; action: SpecAlternativesActionRecord }> {
    // The stage rows have no separate transcript delivery: the canvas reads
    // them through the alternatives route, the same way a tracked edit returns
    // its chip to the caller.
    const result = await this.pool.query(
      `INSERT INTO spec_transcript_action
         (id, spec_id, section_id, request_fingerprint, chip, created_at, delivered_at)
       VALUES ($1, $2, $3, $4, $5, $6, NULL)
       ON CONFLICT (id) DO NOTHING
       RETURNING id`,
      [
        input.id,
        input.specId,
        input.sectionId,
        input.requestFingerprint,
        input.chip,
        input.createdAt,
      ],
    );
    if (result.rowCount === 1) return { status: "stored", action: input };
    const existing = await this.readAction(input.id);
    if (!existing) throw new Error("The concurrent alternatives action is missing.");
    return { status: "replayed", action: existing };
  }
}

/** Newest proposal first; a decision counts only for that proposal. */
export function stageFromRows(
  rows: readonly SpecAlternativesActionRecord[],
): SpecAlternativesStage | null {
  const proposal = rows.find(
    (row): row is SpecAlternativesActionRecord & { chip: AlternativesProposedTranscriptChip } =>
      row.chip.kind === "spec_alternatives_proposed",
  );
  if (!proposal) return null;
  const decision =
    rows.find(
      (row): row is SpecAlternativesActionRecord & { chip: AlternativesDecidedTranscriptChip } =>
        row.chip.kind === "spec_alternatives_decided" &&
        row.chip.setId === proposal.chip.setId,
    ) ?? null;
  return { proposal: proposal.chip, decision: decision?.chip ?? null };
}

function actionValue(row: AlternativesActionRow): SpecAlternativesActionRecord {
  return {
    id: row.id,
    specId: row.spec_id,
    sectionId: row.section_id,
    requestFingerprint: row.request_fingerprint,
    chip: row.chip,
    createdAt: row.created_at,
  };
}

export interface SpecAlternativesServiceOptions {
  store: SpecAlternativesStore;
  documents: Pick<SpecDocumentService, "syncFromLog" | "mutateDocument">;
  now: () => Date;
}

export interface ProposeAlternativesInput extends SpecAlternativesProposal {
  specId: string;
  sectionId: string;
  /** Stable per tool call, so a replayed call stores one set. */
  actionId: string;
}

export interface DecideAlternativeInput {
  specId: string;
  /** The decision is keyed by this set, so it needs no separate action ID. */
  setId: string;
  /** The winning card, or null for an author-written hybrid. */
  optionKey: string | null;
  reason: string;
  decidedBy: "agent" | "author";
  /** Write the section only when the live document is at this revision. */
  expectedRev?: bigint;
}

export interface DecideAlternativeResult {
  stage: SpecAlternativesStage;
  /** False when the section already held this decision. */
  applied: boolean;
  newRev: bigint;
}

export class SpecAlternativesService {
  constructor(private readonly options: SpecAlternativesServiceOptions) {}

  readStage(specId: string): Promise<SpecAlternativesStage | null> {
    return this.options.store.readStage(specId);
  }

  async propose(input: ProposeAlternativesInput): Promise<AlternativesProposedTranscriptChip> {
    validateSpecAlternatives(input);
    const loaded = await this.options.documents.syncFromLog(input.specId);
    requireAlternativesSection(proseMirrorDocument(loaded.doc), input.sectionId);
    const chip: AlternativesProposedTranscriptChip = {
      kind: "spec_alternatives_proposed",
      specId: input.specId,
      sectionId: input.sectionId,
      setId: input.actionId,
      options: input.options,
      comparison: input.comparison,
      leanKey: input.leanKey ?? null,
    };
    const fingerprint = actionFingerprint(chip);
    const result = await this.options.store.insertAction({
      id: proposalActionId(input.specId, input.actionId),
      specId: input.specId,
      sectionId: input.sectionId,
      requestFingerprint: fingerprint,
      chip,
      createdAt: this.options.now(),
    });
    const stored = result.action;
    if (stored.requestFingerprint !== fingerprint) {
      throw new SpecAlternativesConflictError(
        "The alternatives action ID belongs to a different proposal.",
      );
    }
    if (stored.chip.kind !== "spec_alternatives_proposed") {
      throw new SpecAlternativesConflictError(
        "The alternatives action ID belongs to a decision.",
      );
    }
    return stored.chip;
  }

  /**
   * Record the pick and write §Alternatives considered.
   *
   * The decision row is stored first, then the section is written with a
   * content comparison. A process that stops between the two leaves a replay
   * that still writes the section, so the pair converges without a shared
   * transaction.
   *
   * A stale `expectedRev` refuses before anything is stored. A concurrent edit
   * that lands between the store and the write returns `applied: false` with
   * the live revision, and the next call at that revision completes the write.
   */
  async decide(input: DecideAlternativeInput): Promise<DecideAlternativeResult> {
    const stage = await this.options.store.readStage(input.specId);
    if (!stage) throw new SpecAlternativesError("This spec has no alternatives to pick from.");
    if (stage.proposal.setId !== input.setId) {
      throw new SpecAlternativesConflictError(
        "The pick names an alternatives set that is no longer current.",
      );
    }
    if (
      input.optionKey !== null &&
      !stage.proposal.options.some((option) => option.key === input.optionKey)
    ) {
      throw new SpecAlternativesError(`The pick names an unknown option: ${input.optionKey}.`);
    }
    const reason = input.reason.trim();
    if (reason.length === 0) {
      throw new SpecAlternativesError("A pick must state why the winner won.");
    }
    if (reason.length > SPEC_ALTERNATIVES_REASON_MAX_CHARS) {
      throw new SpecAlternativesError(
        `A pick reason is limited to ${SPEC_ALTERNATIVES_REASON_MAX_CHARS} characters.`,
      );
    }
    const current = await this.options.documents.syncFromLog(input.specId);
    if (input.expectedRev !== undefined && input.expectedRev !== current.semanticDocSeq) {
      return {
        stage,
        applied: false,
        newRev: current.semanticDocSeq,
      };
    }
    const chip: AlternativesDecidedTranscriptChip = {
      kind: "spec_alternatives_decided",
      specId: input.specId,
      sectionId: stage.proposal.sectionId,
      setId: input.setId,
      pickedKey: input.optionKey,
      reason,
      decidedBy: input.decidedBy,
    };
    const decision = await this.storeDecision(input, chip);
    const markdown = renderAlternativesConsidered(stage.proposal, decision);
    const write = await this.writeSection(
      input.specId,
      stage.proposal.sectionId,
      markdown,
      input.expectedRev,
    );
    return { stage: { proposal: stage.proposal, decision }, ...write };
  }

  private async storeDecision(
    input: DecideAlternativeInput,
    chip: AlternativesDecidedTranscriptChip,
  ): Promise<AlternativesDecidedTranscriptChip> {
    const fingerprint = actionFingerprint(chip);
    const result = await this.options.store.insertAction({
      id: decisionActionId(input.specId, input.setId),
      specId: input.specId,
      sectionId: chip.sectionId,
      requestFingerprint: fingerprint,
      chip,
      createdAt: this.options.now(),
    });
    const stored = result.action;
    if (stored.chip.kind !== "spec_alternatives_decided") {
      throw new SpecAlternativesConflictError("The decision action ID belongs to a proposal.");
    }
    if (stored.requestFingerprint !== fingerprint) {
      // A second, different pick for the same set. The first pick is the one
      // the section holds, so say so rather than overwrite it silently.
      throw new SpecAlternativesConflictError(
        `This set was already decided: ${describeDecision(stored.chip)}.`,
      );
    }
    return stored.chip;
  }

  private async writeSection(
    specId: string,
    sectionId: string,
    markdown: string,
    expectedRev?: bigint,
  ): Promise<{ applied: boolean; newRev: bigint }> {
    const loaded = await this.options.documents.syncFromLog(specId);
    const document = proseMirrorDocument(loaded.doc);
    const desired = replaceSection(
      document,
      sectionId,
      replacementSection(document, sectionId, markdown),
    );
    if (document.eq(desired)) return { applied: false, newRev: loaded.semanticDocSeq };
    try {
      const update = await this.options.documents.mutateDocument(
        specId,
        "spec-alternatives",
        (doc) => replaceSection(doc, sectionId, replacementSection(doc, sectionId, markdown)),
        expectedRev,
      );
      return { applied: true, newRev: update.semanticDocSeq };
    } catch (error) {
      if (!(error instanceof SpecDocumentRevisionConflictError)) throw error;
      return { applied: false, newRev: error.actualSeq };
    }
  }
}

function describeDecision(chip: AlternativesDecidedTranscriptChip): string {
  return chip.pickedKey === null ? "a hybrid" : `option ${chip.pickedKey}`;
}

export function proposalActionId(specId: string, setId: string): string {
  return `alternatives-set:${specId}:${setId}`;
}

export function decisionActionId(specId: string, setId: string): string {
  return `alternatives-pick:${specId}:${setId}`;
}

function actionFingerprint(chip: object): string {
  return createHash("sha256").update(JSON.stringify(canonicalValue(chip))).digest("hex");
}

function canonicalValue(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonicalValue);
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((key) => [key, canonicalValue(Reflect.get(value, key))]),
    );
  }
  return value;
}

function requireSection(document: ProseMirrorNode, sectionId: string): ProseMirrorNode {
  const found = findSection(document, sectionId);
  if (!found) throw new SpecAlternativesError(`Unknown spec section: ${sectionId}`);
  return found.node;
}

/**
 * A set belongs to the template's alternatives section and to no other. Without
 * this bind, a wrong section id would overwrite that section with the canonical
 * §Alternatives considered body, and its decision would release the Layer-3
 * gate that the genuine section still holds.
 */
function requireAlternativesSection(document: ProseMirrorNode, sectionId: string): void {
  const section = requireSection(document, sectionId);
  if (section.attrs.templateSectionKey !== SPEC_ALTERNATIVES_SECTION_KEY) {
    throw new SpecAlternativesError(
      `Spec section ${sectionId} is not the alternatives section.`,
    );
  }
}

/** The document's alternatives section, or null when the template has none. */
export function findAlternativesSectionId(document: ProseMirrorNode): string | null {
  let found: string | null = null;
  document.forEach((section) => {
    if (
      found === null &&
      section.type === schema.nodes.section &&
      section.attrs.templateSectionKey === SPEC_ALTERNATIVES_SECTION_KEY &&
      typeof section.attrs.id === "string"
    ) {
      found = section.attrs.id;
    }
  });
  return found;
}

function replacementSection(
  document: ProseMirrorNode,
  sectionId: string,
  markdown: string,
): ProseMirrorNode {
  const section = requireSection(document, sectionId);
  const heading = section.firstChild;
  if (!heading || heading.type !== schema.nodes.sectionHeading) {
    throw new SpecAlternativesError(`Spec section ${sectionId} has no stable heading.`);
  }
  return section.type.create(section.attrs, [heading, ...parseMarkdownBlocks(markdown)]);
}
