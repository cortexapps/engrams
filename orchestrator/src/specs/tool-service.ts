import { createHash } from "node:crypto";

import {
  createSectionRelativeAnchor,
  findQuestionMarker,
  findSection,
  isRangeInSectionBody,
  parseSectionRelativeAnchor,
  parseMarkdownBlocks,
  parseSectionBody,
  renderMarkdown,
  replaceSection,
  resolveSectionRelativeAnchor,
  schema,
  selectionSliceFingerprint,
  serializeSectionRelativeAnchor,
  type SpecSelectionSpan,
  type TrackedEditTranscriptChip,
} from "@engrams/spec-document";
import { Fragment, Slice, type Node as ProseMirrorNode } from "prosemirror-model";
import { Transform } from "prosemirror-transform";
import type { Pool } from "pg";
import type * as Y from "yjs";

import type { SpecTemplateSection } from "../db/schema.ts";
import type {
  LiveSpecRead,
  SpecMutationContext,
  SpecMutationResult,
  SpecToolDocumentService,
} from "../tools/specs.ts";
import {
  proseMirrorDocument,
  SpecDocumentRevisionConflictError,
  type SpecTrackedEditActionRecord,
  type SpecDocumentService,
} from "./doc-service.ts";
import {
  OpenQuestionError,
  type OpenQuestionRecord,
  type OpenQuestionService,
  type OpenQuestionStore,
} from "./open-questions.ts";
import { SectionStateConflictError, type SectionStateService } from "./section-state-service.ts";
import type { SpecTicketTreeService } from "./ticket-tree.ts";

const AGENT_QUESTION_NAMESPACE = "6a7dd40c-5d36-529d-9561-5f8e1fbea3c7";
const BLOCK_CHECKPOINT_NAMESPACE = "21b9e56c-54b5-5f8f-a650-320e68aa50d6";

export interface SpecToolMetadataStore {
  phase(specId: string): Promise<"ideation" | "drafting" | "published">;
  templateSections(specId: string): Promise<readonly SpecTemplateSection[]>;
  concurrentEditorNames(specId: string, actorUserId?: string): Promise<string[]>;
}

interface TemplateSectionsRow {
  sections: SpecTemplateSection[];
}

interface SpecPhaseRow {
  phase: string;
}

interface EditorNameRow {
  name: string;
}

export class PostgresSpecToolMetadataStore implements SpecToolMetadataStore {
  constructor(
    private readonly pool: Pool,
    private readonly now: () => Date,
  ) {}

  async phase(specId: string): Promise<"ideation" | "drafting" | "published"> {
    const result = await this.pool.query<SpecPhaseRow>("SELECT phase FROM spec WHERE id = $1", [
      specId,
    ]);
    const value = result.rows[0]?.phase;
    if (value === "ideation" || value === "drafting" || value === "published") return value;
    if (value === undefined) throw new Error(`Unknown spec: ${specId}`);
    throw new Error(`Spec ${specId} has an invalid phase: ${value}`);
  }

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
  metadata: SpecToolMetadataStore;
  /** The post-publish ticket tree, which reads the pinned spec (#1127). */
  tickets: Pick<SpecTicketTreeService, "propose">;
  now: () => Date;
}

export class SpecIdeationPhaseError extends Error {
  constructor(
    readonly newRev: bigint,
    readonly concurrentEditors: string[],
  ) {
    super(
      "This spec is in ideation. Ask the person to start drafting, and keep reading the repository and investigating in the meantime.",
    );
    this.name = "SpecIdeationPhaseError";
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

  private async requireDrafting(specId: string, input: SpecMutationContext): Promise<void> {
    if ((await this.options.metadata.phase(specId)) !== "ideation") return;
    const loaded = await this.options.documents.syncFromLog(specId);
    throw new SpecIdeationPhaseError(
      loaded.semanticDocSeq,
      await this.options.metadata.concurrentEditorNames(specId, input.actorUserId),
    );
  }

  async read(specId: string, sectionId?: string): Promise<LiveSpecRead> {
    const loaded = await this.options.documents.syncFromLog(specId);
    const document = proseMirrorDocument(loaded.doc);
    // The rendered markdown carries no section ids, and every mutation
    // requires one, so the section map rides EVERY read — without it an
    // agent has no way to learn the ids and cannot write at all. The open
    // questions ride along for the same reason: their ledger ids are what
    // spec_resolve_open_question takes, and an agent that cannot list them
    // re-raises duplicates instead of resolving.
    const sections = documentSections(document);
    const openQuestions = (await this.options.questionStore.listOpenBySpec(specId)).map(
      (question) => ({
        id: question.id,
        sectionId: question.sectionId,
        text: question.text,
      }),
    );
    if (sectionId === undefined) {
      return {
        specId,
        rev: loaded.semanticDocSeq,
        markdown: renderMarkdown(document),
        sections,
        openQuestions,
      };
    }
    const section = requireSection(document, sectionId);
    const sectionDocument = schema.nodes.doc!.create(null, section.node);
    return {
      specId,
      rev: loaded.semanticDocSeq,
      markdown: renderMarkdown(sectionDocument),
      sectionId,
      sections,
      openQuestions,
    };
  }

  /**
   * The section-scoped write fence (ADR 0114 amendment).
   *
   * A mutation carries `expected_rev` from the agent's last read. The write
   * is stale only when the TARGET section changed past that revision by
   * someone other than this agent's session (a human, or a system write such
   * as a restore or a distillation). A global-revision fence would bounce
   * every write while a human types anywhere in the document, and the model
   * would learn to rubber-stamp the returned revision — which is no fence.
   *
   * The check runs before the mutation, outside the apply lock: a same-section
   * write that lands in the milliseconds between the check and the apply is
   * not caught. The window it closes is the model's seconds-long think time
   * between its read and its write, which is where clobbers actually happen.
   */
  private async staleForSection(
    specId: string,
    input: SpecMutationContext,
    sectionId: string,
    headRev: bigint,
  ): Promise<boolean> {
    if (input.expectedRev === undefined || input.expectedRev >= headRev) return false;
    const changed = await this.options.documents.sectionsChangedSince(
      specId,
      input.expectedRev,
      `agent:${input.sessionId}:%`,
    );
    return changed.has(sectionId);
  }

  async updateSection(
    specId: string,
    input: SpecMutationContext & {
      sectionId: string;
      markdown: string;
      selection?: SpecSelectionSpan;
    },
  ): Promise<SpecMutationResult> {
    await this.requireDrafting(specId, input);
    if (input.selection) {
      return this.updateSelectedRange(specId, { ...input, selection: input.selection });
    }
    const loaded = await this.options.documents.syncFromLog(specId);
    if (await this.staleForSection(specId, input, input.sectionId, loaded.semanticDocSeq)) {
      return this.result(specId, input, false, loaded.semanticDocSeq);
    }
    const currentDocument = proseMirrorDocument(loaded.doc);
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
          actorUserId: input.actorUserId ?? null,
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
      state: "open" | "proposed" | "settled" | "n/a";
      reason?: string;
    },
  ): Promise<SpecMutationResult> {
    await this.requireDrafting(specId, input);
    const loaded = await this.options.documents.syncFromLog(specId);
    // Changing the state of content that the agent has not seen is the
    // same clobber as a stale section write, so the same fence applies.
    if (await this.staleForSection(specId, input, input.sectionId, loaded.semanticDocSeq)) {
      return this.result(specId, input, false, loaded.semanticDocSeq);
    }
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
    try {
      await this.options.sectionStates.transitionDeferred({
        actionId: `agent-section-state:${specId}:${input.sessionId}:${input.toolCallId}`,
        context: {
          specId,
          sectionId: input.sectionId,
          sectionTitle: section.title,
          allowsNa: templateSection.allowNa,
        },
        target: input.state,
        ...(input.reason === undefined ? {} : { naReason: input.reason }),
        actorUserId: input.actorUserId ?? null,
        // No expectedDocSeq: the store's global-revision check is replaced by
        // the section-scoped fence above, which does not bounce on unrelated
        // edits elsewhere in the document.
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
    await this.requireDrafting(specId, input);
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
    await this.requireDrafting(specId, input);
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
        resolvedBy: input.actorUserId ?? null,
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
    await this.requireDrafting(specId, input);
    const loaded = await this.options.documents.syncFromLog(specId);
    if (await this.staleForSection(specId, input, input.sectionId, loaded.semanticDocSeq)) {
      return this.result(specId, input, false, loaded.semanticDocSeq);
    }
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
    await this.requireDrafting(specId, input);
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
  if (!section) {
    // Teach instead of stonewalling: a wrong guess (a slug, a title) is an
    // agent that never learned the ids, so the rejection carries them.
    const known = documentSections(document)
      .map((candidate) => `${candidate.id} (${candidate.title})`)
      .join(", ");
    throw new Error(`Unknown spec section: ${sectionId}. Valid section ids: ${known}`);
  }
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
  const body = parseSectionBody(markdown, heading.textContent);
  return section.type.create(section.attrs, [
    heading,
    ...preserveQuestionMarkers(section, body),
  ]);
}

/**
 * Carry unresolved question markers a full-section rewrite left out.
 *
 * A rewrite that drops a marker orphans its ledger row: the row stays open
 * and counted, but no card renders and nothing can resolve it. The first live
 * drive hit exactly this — the agent's rewrite deleted two markers, it
 * re-raised both under new ids, and the spec carried two unresolvable
 * duplicates into publish. A marker leaves the document through resolution
 * (or an explicit dismissal), never through a rewrite.
 */
function preserveQuestionMarkers(
  section: ProseMirrorNode,
  body: ProseMirrorNode[],
): ProseMirrorNode[] {
  const kept = new Set<string>();
  for (const block of body) {
    block.descendants((node) => {
      if (node.type === schema.nodes.openQuestion) kept.add(String(node.attrs.questionId));
      return true;
    });
  }
  const dropped: ProseMirrorNode[] = [];
  section.descendants((node) => {
    if (
      node.type === schema.nodes.openQuestion &&
      node.attrs.resolved !== true &&
      !kept.has(String(node.attrs.questionId))
    ) {
      dropped.push(node);
    }
    return true;
  });
  if (dropped.length === 0) return body;
  const last = body[body.length - 1];
  if (last && last.type === schema.nodes.paragraph) {
    return [
      ...body.slice(0, -1),
      last.type.create(last.attrs, last.content.append(Fragment.fromArray(dropped))),
    ];
  }
  return [...body, schema.nodes.paragraph!.create(null, Fragment.fromArray(dropped))];
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
