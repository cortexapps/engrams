import { createHash } from "node:crypto";

import {
  createSectionRelativeAnchor,
  findQuestionMarker,
  findSection,
  isRangeInSectionBody,
  parseSectionRelativeAnchor,
  parseMarkdownBlocks,
  renderMarkdown,
  replaceSection,
  resolveSectionRelativeAnchor,
  schema,
  selectionSliceFingerprint,
  serializeSectionRelativeAnchor,
  SPEC_ALTERNATIVES_SECTION_KEY,
  type SpecAlternativeOption,
  type SpecAlternativesComparison,
  type SpecSelectionSpan,
  type SpecWorkingNotesInput,
  type TrackedEditTranscriptChip,
} from "@engrams/spec-document";
import { Fragment, Slice, type Node as ProseMirrorNode } from "prosemirror-model";
import { Transform } from "prosemirror-transform";
import type { Pool } from "pg";
import type * as Y from "yjs";

import type { SpecTemplateSection, SpecTemplateStageFlags } from "../db/schema.ts";
import type {
  LiveSpecRead,
  SpecAlternativesProposalResult,
  SpecMutationContext,
  SpecProposalContext,
  SpecMutationResult,
  SpecNotesDistillResult,
  SpecNotesUpdateResult,
  SpecToolDocumentService,
} from "../tools/specs.ts";
import type { SpecAlternativesService } from "./alternatives.ts";
import {
  proseMirrorDocument,
  readSpecWorkingNotes,
  specNotesArchivedAt,
  SpecDocumentRevisionConflictError,
  type SpecTrackedEditActionRecord,
  type SpecDocumentService,
} from "./doc-service.ts";
import type { SpecWorkingNotesService } from "./notes.ts";
import {
  OpenQuestionError,
  type OpenQuestionRecord,
  type OpenQuestionService,
  type OpenQuestionStore,
} from "./open-questions.ts";
import { SectionStateConflictError, type SectionStateService } from "./section-state-service.ts";
import type { SpecTicketTreeService } from "./ticket-tree.ts";
import type { SectionStateValue } from "./section-state.ts";

const AGENT_QUESTION_NAMESPACE = "6a7dd40c-5d36-529d-9561-5f8e1fbea3c7";
const BLOCK_CHECKPOINT_NAMESPACE = "21b9e56c-54b5-5f8f-a650-320e68aa50d6";

export interface SpecToolMetadataStore {
  templateSections(specId: string): Promise<readonly SpecTemplateSection[]>;
  templateStageFlags(specId: string): Promise<SpecTemplateStageFlags>;
  sectionStates(specId: string): Promise<ReadonlyMap<string, SectionStateValue>>;
  concurrentEditorNames(specId: string, actorUserId?: string): Promise<string[]>;
}

interface TemplateSectionsRow {
  sections: SpecTemplateSection[];
}

interface TemplateStageFlagsRow {
  stage_flags: SpecTemplateStageFlags;
}

interface SectionStateRow {
  section_id: string;
  state: SectionStateValue["state"];
  na_reason: string | null;
}

interface EditorNameRow {
  name: string;
}

export class PostgresSpecToolMetadataStore implements SpecToolMetadataStore {
  constructor(
    private readonly pool: Pool,
    private readonly now: () => Date,
  ) {}

  async templateSections(specId: string): Promise<readonly SpecTemplateSection[]> {
    const result = await this.pool.query<TemplateSectionsRow>(
      `SELECT t.sections
         FROM spec s
         JOIN spec_template t ON t.id = s.template_id
        WHERE s.id = $1`,
      [specId],
    );
    const row = result.rows[0];
    if (!row) throw new Error(`Unknown spec: ${specId}`);
    return row.sections;
  }

  async templateStageFlags(specId: string): Promise<SpecTemplateStageFlags> {
    const result = await this.pool.query<TemplateStageFlagsRow>(
      `SELECT t.stage_flags
         FROM spec s
         JOIN spec_template t ON t.id = s.template_id
        WHERE s.id = $1`,
      [specId],
    );
    const row = result.rows[0];
    if (!row) throw new Error(`Unknown spec: ${specId}`);
    return row.stage_flags;
  }

  async sectionStates(specId: string): Promise<ReadonlyMap<string, SectionStateValue>> {
    const result = await this.pool.query<SectionStateRow>(
      `SELECT section_id, state, na_reason
         FROM spec_section_state
        WHERE spec_id = $1`,
      [specId],
    );
    return new Map(
      result.rows.map((row) => [row.section_id, { state: row.state, naReason: row.na_reason }]),
    );
  }

  async concurrentEditorNames(specId: string, actorUserId?: string): Promise<string[]> {
    const result = await this.pool.query<EditorNameRow>(
      `SELECT DISTINCT u.name
         FROM spec_participant p
         JOIN "user" u ON u.id = p.user_id
        WHERE p.spec_id = $1
          AND p.disconnected_at IS NULL
          AND p.lease_expires_at > $3
          AND ($2::text IS NULL OR p.user_id <> $2)
        ORDER BY u.name`,
      [specId, actorUserId ?? null, this.now()],
    );
    return result.rows.map((row) => row.name);
  }
}

export interface SpecToolServiceOptions {
  documents: SpecDocumentService;
  sectionStates: SectionStateService;
  questions: OpenQuestionService;
  questionStore: OpenQuestionStore;
  alternatives: SpecAlternativesService;
  notes: Pick<SpecWorkingNotesService, "update" | "distill" | "readStage">;
  metadata: SpecToolMetadataStore;
  /** The post-publish ticket tree, which reads the pinned spec (#1127). */
  tickets: Pick<SpecTicketTreeService, "propose">;
  now: () => Date;
}

/** The template runs the alternatives stage and the pick has not landed. */
export class SpecAlternativesStageError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SpecAlternativesStageError";
  }
}

/** The working notes are open, or the template never runs the stage. */
export class SpecNotesStageError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SpecNotesStageError";
  }
}

interface LocatedDiagramBlock {
  node: ProseMirrorNode;
  position: number;
  sectionId: string;
}

/** Production adapter from agent tool contracts to the collaborative document services. */
export class SpecToolService implements SpecToolDocumentService {
  constructor(private readonly options: SpecToolServiceOptions) {}

  async read(specId: string, sectionId?: string): Promise<LiveSpecRead> {
    const loaded = await this.options.documents.syncFromLog(specId);
    const document = proseMirrorDocument(loaded.doc);
    if (sectionId === undefined) {
      return { specId, rev: loaded.semanticDocSeq, markdown: renderMarkdown(document) };
    }
    const section = requireSection(document, sectionId);
    const sectionDocument = schema.nodes.doc!.create(null, section.node);
    return {
      specId,
      rev: loaded.semanticDocSeq,
      markdown: renderMarkdown(sectionDocument),
      sectionId,
    };
  }

  async updateSection(
    specId: string,
    input: SpecMutationContext & {
      sectionId: string;
      markdown: string;
      selection?: SpecSelectionSpan;
    },
  ): Promise<SpecMutationResult> {
    if (input.selection) {
      return this.updateSelectedRange(specId, { ...input, selection: input.selection });
    }
    const loaded = await this.options.documents.syncFromLog(specId);
    if (input.expectedRev !== undefined && input.expectedRev !== loaded.semanticDocSeq) {
      return this.result(specId, input, false, loaded.semanticDocSeq);
    }
    const currentDocument = proseMirrorDocument(loaded.doc);
    this.assertNotesStageClear(loaded.doc);
    await this.assertAlternativesStageClear(specId, currentDocument, input.sectionId);
    const desired = replaceSection(
      currentDocument,
      input.sectionId,
      replacementSection(currentDocument, input.sectionId, input.markdown),
    );
    if (currentDocument.eq(desired)) {
      return this.result(specId, input, true, loaded.semanticDocSeq);
    }
    try {
      const update = await this.options.documents.mutateDocument(
        specId,
        agentClientId(input),
        (document) =>
          replaceSection(
            document,
            input.sectionId,
            replacementSection(document, input.sectionId, input.markdown),
          ),
        input.expectedRev,
      );
      return this.result(specId, input, true, update.semanticDocSeq);
    } catch (error) {
      return this.revisionConflict(specId, input, error);
    }
  }

  private async updateSelectedRange(
    specId: string,
    input: SpecMutationContext & {
      sectionId: string;
      markdown: string;
      selection: SpecSelectionSpan;
    },
  ): Promise<SpecMutationResult> {
    const actionId = trackedEditActionId(specId, input.sessionId, input.toolCallId);
    const requestFingerprint = trackedEditRequestFingerprint(specId, input);
    const existing = await this.options.documents.readTrackedEditAction(actionId);
    if (existing)
      return storedTrackedEditResult(existing, specId, input.sectionId, requestFingerprint);
    if (input.selection.specId !== specId) {
      throw new Error("The selected range belongs to a different spec.");
    }
    if (input.selection.sectionId !== input.sectionId) {
      throw new Error("The selected range belongs to a different section.");
    }
    const transcriptChip: TrackedEditTranscriptChip = {
      kind: "spec_tracked_edit",
      specId,
      sectionId: input.sectionId,
      before: input.selection.selectedText,
      after: input.markdown,
    };
    const concurrentEditors = await this.options.metadata.concurrentEditorNames(
      specId,
      input.actorUserId,
    );
    try {
      const result = await this.options.documents.mutateDocumentWithTrackedEdit(
        specId,
        agentClientId(input),
        {
          id: actionId,
          specId,
          sectionId: input.sectionId,
          requestFingerprint,
          chip: transcriptChip,
          concurrentEditors,
        },
        (document, ydoc) => replaceSelectedRange(document, ydoc, input.selection, input.markdown),
        input.expectedRev,
      );
      return storedTrackedEditResult(result.action, specId, input.sectionId, requestFingerprint);
    } catch (error) {
      return this.revisionConflict(specId, input, error);
    }
  }

  async setSectionState(
    specId: string,
    input: SpecMutationContext & {
      sectionId: string;
      state: "drafted" | "confirmed" | "n/a";
      reason?: string;
    },
  ): Promise<SpecMutationResult> {
    const loaded = await this.options.documents.syncFromLog(specId);
    const document = proseMirrorDocument(loaded.doc);
    const sections = documentSections(document);
    const sectionIndex = sections.findIndex((section) => section.id === input.sectionId);
    if (sectionIndex < 0) throw new Error(`Unknown spec section: ${input.sectionId}`);
    const section = sections[sectionIndex]!;
    const templateSections = await this.options.metadata.templateSections(specId);
    const templateSection = templateSections.find((candidate) => candidate.key === section.key);
    if (!templateSection) {
      throw new Error(`Spec section ${input.sectionId} has no template rule.`);
    }
    const states = await this.options.metadata.sectionStates(specId);
    const unconfirmedUpstreamSectionIds = sections
      .slice(0, sectionIndex)
      .filter((candidate) => {
        const state = states.get(candidate.id)?.state ?? "empty";
        return state !== "confirmed" && state !== "n/a";
      })
      .map((candidate) => candidate.id);
    try {
      await this.options.sectionStates.transitionDeferred({
        actionId: `agent-section-state:${specId}:${input.sessionId}:${input.toolCallId}`,
        context: {
          specId,
          sectionId: input.sectionId,
          sectionTitle: section.title,
          allowsNa: templateSection.allowNa,
          unconfirmedUpstreamSectionIds,
        },
        target: input.state,
        ...(input.reason === undefined ? {} : { naReason: input.reason }),
        actorUserId: input.actorUserId ?? null,
        ...(input.expectedRev === undefined ? {} : { expectedDocSeq: input.expectedRev }),
      });
      const latest = await this.options.documents.syncFromLog(specId);
      return this.result(specId, input, true, latest.semanticDocSeq);
    } catch (error) {
      if (!(error instanceof SectionStateConflictError)) throw error;
      const latest = await this.options.documents.syncFromLog(specId);
      return this.result(specId, input, false, latest.semanticDocSeq);
    }
  }

  async addOpenQuestion(
    specId: string,
    input: SpecMutationContext & { sectionId: string; question: string },
  ): Promise<SpecMutationResult> {
    const questionId = stableQuestionId(specId, input.sessionId, input.toolCallId);
    const loaded = await this.options.documents.syncFromLog(specId);
    const document = proseMirrorDocument(loaded.doc);
    requireSection(document, input.sectionId);
    const questionText = input.question.trim();
    if (questionText.length === 0) {
      throw new OpenQuestionError("question_text_required", "An open question must have text.");
    }
    const row = await this.options.questionStore.find(questionId);
    const marker = findQuestionMarker(document, questionId);

    if (row && marker) {
      assertQuestionReplay(row, input, specId);
      if (!findQuestionMarker(document, questionId, input.sectionId)) {
        throw new Error(`Open question ${questionId} belongs to a different section.`);
      }
      assertQuestionMarker(row, marker.node);
      return this.result(specId, input, true, loaded.semanticDocSeq);
    }
    if (row) {
      throw new Error(`Open question ${questionId} has no document marker.`);
    }
    if (marker) {
      if (!findQuestionMarker(document, questionId, input.sectionId)) {
        throw new Error(`Open question ${questionId} belongs to a different section.`);
      }
      if (marker.node.attrs.resolved === true) {
        throw new Error(`Open question ${questionId} is resolved without a ledger row.`);
      }
      const requestFingerprint = marker.node.attrs.requestFingerprint;
      if (typeof requestFingerprint !== "string" || requestFingerprint.length === 0) {
        throw new Error(`Open question ${questionId} has no request fingerprint.`);
      }
      const repaired = await this.options.questionStore.create({
        id: questionId,
        specId,
        sectionId: input.sectionId,
        text: questionText,
        openedBy: input.actorUserId ?? null,
        requestFingerprint,
      });
      assertQuestionReplay(repaired, input, specId);
      if (repaired.requestFingerprint !== requestFingerprint) {
        throw new OpenQuestionError(
          "question_key_conflict",
          "The stable question ID belongs to a different marker.",
        );
      }
      const latest = await this.options.documents.syncFromLog(specId);
      return this.result(specId, input, true, latest.semanticDocSeq);
    }

    const anchorPosition = lastQuestionAnchor(document, input.sectionId);
    const anchor = serializeSectionRelativeAnchor(
      createSectionRelativeAnchor(loaded.doc, input.sectionId, anchorPosition),
    );
    try {
      await this.options.questions.open({
        questionId,
        specId,
        sectionId: input.sectionId,
        text: input.question,
        openedBy: input.actorUserId ?? null,
        anchor,
        ...(input.expectedRev === undefined ? {} : { expectedDocSeq: input.expectedRev }),
      });
      const latest = await this.options.documents.syncFromLog(specId);
      return this.result(specId, input, true, latest.semanticDocSeq);
    } catch (error) {
      return this.revisionConflict(specId, input, error);
    }
  }

  async resolveOpenQuestion(
    specId: string,
    input: SpecMutationContext & {
      sectionId: string;
      questionId: string;
      answerMarkdown: string;
    },
  ): Promise<SpecMutationResult> {
    const question = await this.options.questionStore.find(input.questionId);
    if (!question)
      throw new OpenQuestionError("question_not_found", "The open question does not exist.");
    if (question.specId !== specId || question.sectionId !== input.sectionId) {
      throw new Error("The open question belongs to a different spec section.");
    }
    if (question.state === "resolved") {
      const loaded = await this.options.documents.syncFromLog(specId);
      const marker = findQuestionMarker(
        proseMirrorDocument(loaded.doc),
        input.questionId,
        input.sectionId,
      );
      if (
        !marker ||
        marker.node.attrs.resolved !== true ||
        marker.node.attrs.answerMarkdown !== input.answerMarkdown.trim()
      ) {
        throw new Error("The open question was already resolved with a different answer.");
      }
      return this.result(specId, input, true, loaded.semanticDocSeq);
    }
    try {
      await this.options.questions.resolve({
        questionId: input.questionId,
        answerMarkdown: input.answerMarkdown,
        ...(input.expectedRev === undefined ? {} : { expectedDocSeq: input.expectedRev }),
      });
      const latest = await this.options.documents.syncFromLog(specId);
      return this.result(specId, input, true, latest.semanticDocSeq);
    } catch (error) {
      return this.revisionConflict(specId, input, error);
    }
  }

  async updateBlock(
    specId: string,
    input: SpecMutationContext & { sectionId: string; blockId: string; source: string },
  ): Promise<SpecMutationResult> {
    const loaded = await this.options.documents.syncFromLog(specId);
    const currentDocument = proseMirrorDocument(loaded.doc);
    requireSection(currentDocument, input.sectionId);
    const current = requireDiagramBlock(currentDocument, input.sectionId, input.blockId);
    if (current.node.attrs.source === input.source) {
      return this.result(specId, input, true, loaded.semanticDocSeq);
    }
    try {
      const checkpointId = stableBlockCheckpointId(specId, input.sessionId, input.toolCallId);
      const blockKind = String(current.node.attrs.kind);
      const result = await this.options.documents.mutateDocumentWithPostEditCheckpoint(
        specId,
        agentClientId(input),
        {
          id: checkpointId,
          label: `Updated ${blockKind} block ${input.blockId}`,
          authorUserId: input.actorUserId ?? null,
          reason: "block_edit",
          createdAt: this.options.now(),
        },
        (document) => {
          const block = requireDiagramBlock(document, input.sectionId, input.blockId);
          return new Transform(document).setNodeMarkup(block.position, undefined, {
            ...block.node.attrs,
            source: input.source,
            cachedRender: null,
          }).doc;
        },
        input.expectedRev,
      );
      return this.result(
        specId,
        input,
        true,
        result.update.semanticDocSeq,
        undefined,
        checkpointId,
      );
    } catch (error) {
      return this.revisionConflict(specId, input, error);
    }
  }

  async proposeAlternatives(
    specId: string,
    input: SpecProposalContext & {
      sectionId: string;
      options: SpecAlternativeOption[];
      comparison: SpecAlternativesComparison;
      leanKey: string | null;
    },
  ): Promise<SpecAlternativesProposalResult> {
    const chip = await this.options.alternatives.propose({
      specId,
      sectionId: input.sectionId,
      // A proposal writes no document, so the set is deduplicated by the call
      // that made it rather than by a document revision.
      actionId: `${input.sessionId}:${input.toolCallId}`,
      options: input.options,
      comparison: input.comparison,
      leanKey: input.leanKey,
    });
    const latest = await this.options.documents.syncFromLog(specId);
    return {
      ...(await this.result(specId, input, true, latest.semanticDocSeq)),
      setId: chip.setId,
    };
  }

  async decideAlternative(
    specId: string,
    input: SpecMutationContext & { setId: string; optionKey: string | null; reason: string },
  ): Promise<SpecMutationResult> {
    const decision = await this.options.alternatives.decide({
      specId,
      setId: input.setId,
      optionKey: input.optionKey,
      reason: input.reason,
      decidedBy: "agent",
      ...(input.expectedRev === undefined ? {} : { expectedRev: input.expectedRev }),
    });
    return this.result(specId, input, decision.applied, decision.newRev);
  }

  /**
   * Deep Layer-3 drafting waits for the pick when the template runs the
   * alternatives stage (R20). The alternatives section itself stays writable,
   * and a confirmed or n/a alternatives section releases the rest of the layer
   * for a spec that settled the question by hand.
   */
  private async assertAlternativesStageClear(
    specId: string,
    document: ProseMirrorNode,
    sectionId: string,
  ): Promise<void> {
    const flags = await this.options.metadata.templateStageFlags(specId);
    if (flags.alternatives !== "on") return;
    const templateSections = await this.options.metadata.templateSections(specId);
    const alternativesRule = templateSections.find(
      (candidate) => candidate.key === SPEC_ALTERNATIVES_SECTION_KEY,
    );
    if (!alternativesRule) return;
    const sections = documentSections(document);
    const target = sections.find((candidate) => candidate.id === sectionId);
    if (!target || target.key === SPEC_ALTERNATIVES_SECTION_KEY) return;
    const targetRule = templateSections.find((candidate) => candidate.key === target.key);
    if (!targetRule || targetRule.layerKey !== alternativesRule.layerKey) return;
    const alternativesSection = sections.find(
      (candidate) => candidate.key === SPEC_ALTERNATIVES_SECTION_KEY,
    );
    if (!alternativesSection) return;
    const states = await this.options.metadata.sectionStates(specId);
    const state = states.get(alternativesSection.id)?.state;
    if (state === "confirmed" || state === "n/a") return;
    const stage = await this.options.alternatives.readStage(specId);
    // Only a decision on the real alternatives section releases the layer. A
    // set bound elsewhere must never unblock the gate it bypassed.
    if (stage?.decision && stage.decision.sectionId === alternativesSection.id) return;
    const proposed = stage?.proposal.sectionId === alternativesSection.id;
    throw new SpecAlternativesStageError(
      proposed
        ? `${target.title} waits for the alternatives pick. Ask the author to pick a card, then call spec_decide_alternative.`
        : `${target.title} waits for the alternatives stage. Call spec_propose_alternatives for ${alternativesSection.title} first.`,
    );
  }

  async updateNotes(
    specId: string,
    input: SpecMutationContext & { notes: SpecWorkingNotesInput },
  ): Promise<SpecNotesUpdateResult> {
    await this.assertNotesStageRuns(specId);
    const result = await this.options.notes.update({
      specId,
      notes: input.notes,
      clientId: agentClientId(input),
      ...(input.expectedRev === undefined ? {} : { expectedRev: input.expectedRev }),
    });
    return {
      ...(await this.result(specId, input, true, result.newRev)),
      stage: result.stage,
      corrections: result.corrections,
    };
  }

  async distillNotes(
    specId: string,
    input: SpecMutationContext,
  ): Promise<SpecNotesDistillResult> {
    const result = await this.options.notes.distill({
      specId,
      clientId: agentClientId(input),
      ...(input.expectedRev === undefined ? {} : { expectedRev: input.expectedRev }),
    });
    return {
      ...(await this.result(specId, input, result.applied, result.newRev)),
      distillation: result.distillation,
      stage: result.stage,
    };
  }

  /** The template must run the stage before the agent opens a notes pane. */
  private async assertNotesStageRuns(specId: string): Promise<void> {
    const flags = await this.options.metadata.templateStageFlags(specId);
    if (flags.talkItThrough === "off") {
      throw new SpecNotesStageError("This template does not run the talk-it-through stage.");
    }
  }

  /**
   * Pen down (R21): while the working notes are live, the agent writes no spec
   * section. The canvas hosts the notes instead, and distillation is the one
   * write that closes the stage. Sections written before the stage opened stay
   * as they are, so a recon draft is not blocked by a later conversation, and a
   * scoped edit that a person asked for over a selection still lands: the rule
   * stops the agent's own drafting, not the author's requests.
   */
  private assertNotesStageClear(doc: Y.Doc): void {
    if (readSpecWorkingNotes(doc) === null) return;
    if (specNotesArchivedAt(doc) !== null) return;
    throw new SpecNotesStageError(
      "The working notes are open, so the spec sections are closed. Keep the notes with spec_update_notes, then call spec_distill_notes to write the sections.",
    );
  }

  /**
   * Land the agent's ticket proposal (ADR 0114 D6).
   *
   * The tree is not part of the document, so this tool changes no revision.
   * It still honours `expected_rev`, because an agent that proposes against a
   * spec it has not re-read should learn that before its tickets land.
   */
  async proposeTickets(
    specId: string,
    input: Parameters<SpecToolDocumentService["proposeTickets"]>[1],
  ): Promise<SpecMutationResult> {
    const loaded = await this.options.documents.syncFromLog(specId);
    if (input.expectedRev !== undefined && input.expectedRev !== loaded.semanticDocSeq) {
      return this.result(specId, input, false, loaded.semanticDocSeq);
    }
    await this.options.tickets.propose({
      specId,
      idempotencyKey: input.idempotencyKey,
      tickets: input.tickets,
    });
    return this.result(specId, input, true, loaded.semanticDocSeq);
  }

  private async revisionConflict(
    specId: string,
    input: SpecMutationContext,
    error: unknown,
  ): Promise<SpecMutationResult> {
    if (!(error instanceof SpecDocumentRevisionConflictError)) throw error;
    return this.result(specId, input, false, error.actualSeq);
  }

  private async result(
    specId: string,
    input: SpecMutationContext,
    applied: boolean,
    newRev: bigint,
    transcriptChip?: TrackedEditTranscriptChip,
    checkpointId?: string,
  ): Promise<SpecMutationResult> {
    return {
      applied,
      newRev,
      concurrentEditors: await this.options.metadata.concurrentEditorNames(
        specId,
        input.actorUserId,
      ),
      ...(transcriptChip === undefined ? {} : { transcriptChip }),
      ...(checkpointId === undefined ? {} : { checkpointId }),
    };
  }
}

function agentClientId(input: SpecMutationContext): string {
  return `agent:${input.sessionId}:${input.toolCallId}`;
}

function requireSection(document: ProseMirrorNode, sectionId: string) {
  const section = findSection(document, sectionId);
  if (!section) throw new Error(`Unknown spec section: ${sectionId}`);
  return section;
}

function replacementSection(
  document: ProseMirrorNode,
  sectionId: string,
  markdown: string,
): ProseMirrorNode {
  const section = requireSection(document, sectionId).node;
  const heading = section.firstChild;
  if (!heading || heading.type !== schema.nodes.sectionHeading) {
    throw new Error(`Spec section ${sectionId} has no stable heading.`);
  }
  return section.type.create(section.attrs, [heading, ...parseMarkdownBlocks(markdown)]);
}

/** Replace the exact anchored range. Transform.replace does not expand the range. */
function replaceSelectedRange(
  document: ProseMirrorNode,
  ydoc: Y.Doc,
  selection: SpecSelectionSpan,
  replacementMarkdown: string,
): ProseMirrorNode {
  const startAnchor = parseSectionRelativeAnchor(selection.startAnchor);
  const endAnchor = parseSectionRelativeAnchor(selection.endAnchor);
  if (
    startAnchor.sectionId !== selection.sectionId ||
    endAnchor.sectionId !== selection.sectionId
  ) {
    throw new Error("The selection anchors belong to a different section.");
  }
  const start = resolveSectionRelativeAnchor(ydoc, startAnchor);
  const end = resolveSectionRelativeAnchor(ydoc, endAnchor);
  if (start === null || end === null || start >= end) {
    throw new Error("The selected range is no longer valid.");
  }
  if (!isRangeInSectionBody(document, selection.sectionId, start, end)) {
    throw new Error("The selected range must be inside the section body.");
  }
  const fingerprint = selectionSliceFingerprint(document, start, end);
  if (fingerprint !== selection.sliceFingerprint) {
    throw new Error("The selected structure changed before the scoped edit.");
  }
  const slice = Slice.maxOpen(Fragment.fromArray(parseMarkdownBlocks(replacementMarkdown)));
  return new Transform(document).replace(start, end, slice).doc;
}

function documentSections(document: ProseMirrorNode): Array<{
  id: string;
  key: string;
  title: string;
}> {
  const sections: Array<{ id: string; key: string; title: string }> = [];
  document.forEach((section) => {
    if (section.type !== schema.nodes.section) return;
    const id = section.attrs.id;
    const key = section.attrs.templateSectionKey;
    if (typeof id !== "string" || typeof key !== "string") {
      throw new Error("Every spec section must have a template identity.");
    }
    sections.push({ id, key, title: section.firstChild?.textContent || id });
  });
  return sections;
}

function lastQuestionAnchor(document: ProseMirrorNode, sectionId: string): number {
  const section = requireSection(document, sectionId);
  let result: number | null = null;
  section.node.descendants((node, position) => {
    if (
      node.type !== schema.nodes.sectionHeading &&
      node.isTextblock &&
      node.contentMatchAt(node.childCount).matchType(schema.nodes.openQuestion!) !== null
    ) {
      result = section.position + 1 + position + 1 + node.content.size;
    }
  });
  if (result === null) {
    throw new Error(`Spec section ${sectionId} has no text block for an open question.`);
  }
  return result;
}

function diagramBlocks(document: ProseMirrorNode, blockId: string): LocatedDiagramBlock[] {
  const result: LocatedDiagramBlock[] = [];
  document.forEach((section, sectionPosition) => {
    if (section.type !== schema.nodes.section || typeof section.attrs.id !== "string") return;
    section.descendants((node, position) => {
      if (node.type === schema.nodes.diagramBlock && node.attrs.id === blockId) {
        result.push({
          node,
          position: sectionPosition + 1 + position,
          sectionId: section.attrs.id,
        });
      }
    });
  });
  return result;
}

function requireDiagramBlock(
  document: ProseMirrorNode,
  sectionId: string,
  blockId: string,
): LocatedDiagramBlock {
  const matches = diagramBlocks(document, blockId);
  const scoped = matches.filter((block) => block.sectionId === sectionId);
  if (scoped.length === 1 && matches.length === 1) return scoped[0]!;
  if (scoped.length === 0 && matches.length > 0) {
    throw new Error(`Diagram block ${blockId} belongs to a different section.`);
  }
  if (scoped.length === 0) throw new Error(`Unknown diagram block: ${blockId}`);
  throw new Error(`Diagram block ${blockId} is not unique.`);
}

function assertQuestionReplay(
  row: OpenQuestionRecord,
  input: SpecMutationContext & { sectionId: string; question: string },
  specId: string,
): void {
  if (
    row.specId !== specId ||
    row.sectionId !== input.sectionId ||
    row.text !== input.question.trim() ||
    row.openedBy !== (input.actorUserId ?? null)
  ) {
    throw new OpenQuestionError(
      "question_key_conflict",
      "The stable question ID belongs to a different request.",
    );
  }
}

function assertQuestionMarker(row: OpenQuestionRecord, marker: ProseMirrorNode): void {
  if (marker.attrs.requestFingerprint !== row.requestFingerprint) {
    throw new OpenQuestionError(
      "question_key_conflict",
      "The question row and document marker belong to different requests.",
    );
  }
  if (
    (row.state === "open" && marker.attrs.resolved === true) ||
    (row.state === "resolved" && marker.attrs.resolved !== true)
  ) {
    throw new Error(`Open question ${row.id} has inconsistent row and marker states.`);
  }
}

export function stableQuestionId(specId: string, sessionId: string, toolCallId: string): string {
  return uuidV5(AGENT_QUESTION_NAMESPACE, `${specId}:${sessionId}:${toolCallId}`);
}

export function trackedEditActionId(specId: string, sessionId: string, toolCallId: string): string {
  return `selection-edit:${specId}:${sessionId}:${toolCallId}`;
}

function trackedEditRequestFingerprint(
  specId: string,
  input: SpecMutationContext & {
    sectionId: string;
    markdown: string;
    selection: SpecSelectionSpan;
  },
): string {
  const canonical = canonicalValue({
    kind: "selection_edit",
    specId,
    sessionId: input.sessionId,
    toolCallId: input.toolCallId,
    actorUserId: input.actorUserId ?? null,
    sectionId: input.sectionId,
    markdown: input.markdown,
    expectedRev: input.expectedRev?.toString() ?? null,
    selection: input.selection,
  });
  return createHash("sha256").update(JSON.stringify(canonical)).digest("hex");
}

function storedTrackedEditResult(
  action: SpecTrackedEditActionRecord,
  specId: string,
  sectionId: string,
  requestFingerprint: string,
): SpecMutationResult {
  if (action.specId !== specId || action.sectionId !== sectionId) {
    throw new Error("The tracked-edit action ID belongs to a different spec section.");
  }
  if (action.chip.specId !== action.specId || action.chip.sectionId !== action.sectionId) {
    throw new Error("The tracked-edit action has an invalid transcript scope.");
  }
  if (action.requestFingerprint !== requestFingerprint) {
    throw new Error("The tracked-edit action ID belongs to a different request.");
  }
  if (JSON.stringify(action.chip) !== JSON.stringify(action.result.transcriptChip)) {
    throw new Error("The tracked-edit action result does not match its transcript chip.");
  }
  return {
    applied: action.result.applied,
    newRev: action.result.newRev,
    concurrentEditors: [...action.result.concurrentEditors],
    transcriptChip: action.result.transcriptChip,
  };
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

export function stableBlockCheckpointId(
  specId: string,
  sessionId: string,
  toolCallId: string,
): string {
  return uuidV5(BLOCK_CHECKPOINT_NAMESPACE, `${specId}:${sessionId}:${toolCallId}`);
}

function uuidV5(namespace: string, name: string): string {
  const namespaceBytes = Buffer.from(namespace.replaceAll("-", ""), "hex");
  if (namespaceBytes.length !== 16) throw new Error("The UUID namespace is invalid.");
  const bytes = createHash("sha1")
    .update(namespaceBytes)
    .update(name, "utf8")
    .digest()
    .subarray(0, 16);
  bytes[6] = (bytes[6]! & 0x0f) | 0x50;
  bytes[8] = (bytes[8]! & 0x3f) | 0x80;
  const hex = bytes.toString("hex");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
