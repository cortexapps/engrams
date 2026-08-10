import { createHash } from "node:crypto";

import {
  createSectionRelativeAnchor,
  findQuestionMarker,
  findSection,
  parseMarkdownBlocks,
  renderMarkdown,
  replaceSection,
  schema,
  serializeSectionRelativeAnchor,
} from "@engrams/spec-document";
import type { Node as ProseMirrorNode } from "prosemirror-model";
import { Transform } from "prosemirror-transform";
import type { Pool } from "pg";

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
  type SpecDocumentService,
} from "./doc-service.ts";
import {
  OpenQuestionError,
  type OpenQuestionRecord,
  type OpenQuestionService,
  type OpenQuestionStore,
} from "./open-questions.ts";
import { SectionStateConflictError, type SectionStateService } from "./section-state-service.ts";
import type { SectionStateValue } from "./section-state.ts";

const AGENT_QUESTION_NAMESPACE = "6a7dd40c-5d36-529d-9561-5f8e1fbea3c7";
const BLOCK_CHECKPOINT_NAMESPACE = "21b9e56c-54b5-5f8f-a650-320e68aa50d6";

export interface SpecToolMetadataStore {
  templateSections(specId: string): Promise<readonly SpecTemplateSection[]>;
  sectionStates(specId: string): Promise<ReadonlyMap<string, SectionStateValue>>;
  concurrentEditorNames(specId: string, actorUserId?: string): Promise<string[]>;
}

interface TemplateSectionsRow {
  sections: SpecTemplateSection[];
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
  constructor(private readonly pool: Pool) {}

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

  async sectionStates(specId: string): Promise<ReadonlyMap<string, SectionStateValue>> {
    const result = await this.pool.query<SectionStateRow>(
      `SELECT section_id, state, na_reason
         FROM spec_section_state
        WHERE spec_id = $1`,
      [specId],
    );
    return new Map(
      result.rows.map((row) => [
        row.section_id,
        { state: row.state, naReason: row.na_reason },
      ]),
    );
  }

  async concurrentEditorNames(specId: string, actorUserId?: string): Promise<string[]> {
    const result = await this.pool.query<EditorNameRow>(
      `SELECT DISTINCT u.name
         FROM spec_participant p
         JOIN "user" u ON u.id = p.user_id
        WHERE p.spec_id = $1
          AND p.disconnected_at IS NULL
          AND ($2::text IS NULL OR p.user_id <> $2)
        ORDER BY u.name`,
      [specId, actorUserId ?? null],
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
  now: () => Date;
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
    input: SpecMutationContext & { sectionId: string; markdown: string },
  ): Promise<SpecMutationResult> {
    const loaded = await this.options.documents.syncFromLog(specId);
    const desired = replacementSection(proseMirrorDocument(loaded.doc), input.sectionId, input.markdown);
    if (requireSection(proseMirrorDocument(loaded.doc), input.sectionId).node.eq(desired)) {
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
      throw new OpenQuestionError(
        "question_text_required",
        "An open question must have text.",
      );
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
    if (!question) throw new OpenQuestionError("question_not_found", "The open question does not exist.");
    if (question.specId !== specId || question.sectionId !== input.sectionId) {
      throw new Error("The open question belongs to a different spec section.");
    }
    if (question.state === "resolved") {
      const loaded = await this.options.documents.syncFromLog(specId);
      const marker = findQuestionMarker(proseMirrorDocument(loaded.doc), input.questionId, input.sectionId);
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
      const update = await this.options.documents.mutateDocument(
        specId,
        agentClientId(input),
        (document) => {
          const block = requireDiagramBlock(document, input.sectionId, input.blockId);
          return new Transform(document).setNodeMarkup(block.position, undefined, {
            ...block.node.attrs,
            source: input.source,
            cachedRender: null,
          }).doc;
        },
        input.expectedRev,
        {
          id: checkpointId,
          label: `Updated ${blockKind} block ${input.blockId}`,
          authorUserId: input.actorUserId ?? null,
          reason: "block_edit",
          at: this.options.now(),
        },
      );
      return this.result(specId, input, true, update.semanticDocSeq, checkpointId);
    } catch (error) {
      return this.revisionConflict(specId, input, error);
    }
  }

  async updateNotes(
    _specId: string,
    _input: Parameters<SpecToolDocumentService["updateNotes"]>[1],
  ): Promise<SpecMutationResult> {
    throw new Error("Working notes are not available until #1120.");
  }

  async proposeTickets(
    _specId: string,
    _input: Parameters<SpecToolDocumentService["proposeTickets"]>[1],
  ): Promise<SpecMutationResult> {
    throw new Error("Ticket drafts are not available until #1127.");
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
    checkpointId?: string,
  ): Promise<SpecMutationResult> {
    return {
      applied,
      newRev,
      concurrentEditors: await this.options.metadata.concurrentEditorNames(
        specId,
        input.actorUserId,
      ),
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
  const bytes = createHash("sha1").update(namespaceBytes).update(name, "utf8").digest().subarray(0, 16);
  bytes[6] = (bytes[6]! & 0x0f) | 0x50;
  bytes[8] = (bytes[8]! & 0x3f) | 0x80;
  const hex = bytes.toString("hex");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
