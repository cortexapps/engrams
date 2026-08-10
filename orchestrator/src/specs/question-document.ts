import {
  createSectionRelativeAnchor,
  findQuestionMarker,
  insertQuestionMarker,
  parseSectionRelativeAnchor,
  QuestionResolutionConflictError,
  removeQuestionMarker,
  resolveQuestionMarker,
  resolveSectionRelativeAnchor,
  serializeSectionRelativeAnchor,
} from "@engrams/spec-document";

import type { QuestionDocument } from "./open-questions.ts";
import { proseMirrorDocument, type SpecDocumentService } from "./doc-service.ts";

export class SpecQuestionDocument implements QuestionDocument {
  constructor(
    private readonly documents: SpecDocumentService,
    private readonly clientId = "spec-question-service",
  ) {}

  async addQuestionMarker(input: {
    questionId: string;
    specId: string;
    sectionId: string;
    anchor: string;
    requestFingerprint: string;
    expectedDocSeq?: bigint;
  }): Promise<boolean> {
    const anchor = parseSectionRelativeAnchor(input.anchor);
    if (anchor.sectionId !== input.sectionId) {
      throw new Error("The question anchor belongs to a different section.");
    }
    const loaded = await this.documents.syncFromLog(input.specId);
    const beforeDocument = proseMirrorDocument(loaded.doc);
    const existing = findQuestionMarker(beforeDocument, input.questionId);
    if (existing) {
      if (!findQuestionMarker(beforeDocument, input.questionId, input.sectionId)) {
        throw new Error(`Open question ${input.questionId} belongs to a different request.`);
      }
      assertMarkerRequest(existing.node.attrs, input);
      return false;
    }
    try {
      await this.documents.mutateDocument(
        input.specId,
        this.clientId,
        (doc, ydoc) => {
          const position = resolveSectionRelativeAnchor(ydoc, anchor);
          if (position == null) throw new Error("The question anchor no longer exists.");
          return insertQuestionMarker(
            doc,
            input.sectionId,
            position,
            input.questionId,
            input.requestFingerprint,
          );
        },
        input.expectedDocSeq,
      );
      return true;
    } catch (error) {
      const latest = await this.documents.syncFromLog(input.specId);
      const latestDocument = proseMirrorDocument(latest.doc);
      const replay = findQuestionMarker(latestDocument, input.questionId);
      if (replay) {
        if (!findQuestionMarker(latestDocument, input.questionId, input.sectionId)) {
          throw new Error(`Open question ${input.questionId} belongs to a different request.`);
        }
        assertMarkerRequest(replay.node.attrs, input);
        return false;
      }
      throw error;
    }
  }

  async removeQuestionMarker(input: { questionId: string; specId: string }): Promise<void> {
    const loaded = await this.documents.syncFromLog(input.specId);
    if (!findQuestionMarker(proseMirrorDocument(loaded.doc), input.questionId)) return;
    await this.documents.mutateDocument(input.specId, this.clientId, (doc) =>
      removeQuestionMarker(doc, input.questionId),
    );
  }

  async absorbAnswer(input: {
    questionId: string;
    specId: string;
    sectionId: string;
    answerMarkdown: string;
    expectedDocSeq?: bigint;
  }): Promise<{ changed: boolean; replayed: boolean; resolutionLink: string | null }> {
    const before = await this.documents.syncFromLog(input.specId);
    const existing = resolutionStatus(
      before.doc,
      input.sectionId,
      input.questionId,
      input.answerMarkdown,
    );
    if (existing?.replayed) return existing;

    try {
      await this.documents.mutateDocument(
        input.specId,
        this.clientId,
        (doc) =>
          resolveQuestionMarker(doc, input.sectionId, input.questionId, input.answerMarkdown).doc,
        input.expectedDocSeq,
      );
    } catch (error) {
      // A peer can win after the first log sync. Read the log again. An
      // identical answer is an idempotent replay; all other errors remain real.
      const latest = await this.documents.syncFromLog(input.specId);
      const replay = resolutionStatus(
        latest.doc,
        input.sectionId,
        input.questionId,
        input.answerMarkdown,
      );
      if (replay?.replayed) return replay;
      throw error;
    }

    const updated = await this.documents.syncFromLog(input.specId);
    const status = resolutionStatus(
      updated.doc,
      input.sectionId,
      input.questionId,
      input.answerMarkdown,
    );
    if (!status) throw new Error("The question resolution has no document anchor.");
    return { ...status, changed: true, replayed: false };
  }
}

function assertMarkerRequest(
  attrs: Readonly<Record<string, unknown>>,
  input: { sectionId: string; questionId: string; requestFingerprint: string },
): void {
  if (
    attrs.questionId !== input.questionId ||
    attrs.requestFingerprint !== input.requestFingerprint ||
    attrs.resolved === true
  ) {
    throw new Error(`Open question ${input.questionId} belongs to a different request.`);
  }
}

function resolutionStatus(
  doc: Awaited<ReturnType<SpecDocumentService["loadDoc"]>>["doc"],
  sectionId: string,
  questionId: string,
  answerMarkdown: string,
): { changed: false; replayed: true; resolutionLink: string } | null {
  const proseMirror = proseMirrorDocument(doc);
  const marker = findQuestionMarker(proseMirror, questionId, sectionId);
  if (!marker || marker.node.attrs.resolved !== true) return null;
  if (marker.node.attrs.answerMarkdown !== answerMarkdown) {
    throw new QuestionResolutionConflictError(questionId);
  }
  const anchor = createSectionRelativeAnchor(doc, sectionId, marker.position);
  return {
    changed: false,
    replayed: true,
    resolutionLink: serializeSectionRelativeAnchor(anchor),
  };
}
