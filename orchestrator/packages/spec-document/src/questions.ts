import { Fragment, type Node as ProseMirrorNode } from "prosemirror-model";
import { Transform } from "prosemirror-transform";

import { findSection, parseMarkdownBlocks, schema } from "./schema.ts";

export interface LocatedQuestionMarker {
  node: ProseMirrorNode;
  position: number;
}

export interface ResolveQuestionMarkerResult {
  doc: ProseMirrorNode;
  replayed: boolean;
}

export class QuestionResolutionConflictError extends Error {
  constructor(readonly questionId: string) {
    super(`Open question ${questionId} already has a different answer.`);
    this.name = "QuestionResolutionConflictError";
  }
}

export function findQuestionMarker(
  doc: ProseMirrorNode,
  questionId: string,
  sectionId?: string,
): LocatedQuestionMarker | null {
  const section = sectionId == null ? null : findSection(doc, sectionId);
  if (sectionId != null && !section) return null;
  const start = section?.position ?? 0;
  const end = section ? section.position + section.node.nodeSize : doc.content.size;
  let found: LocatedQuestionMarker | null = null;
  doc.descendants((node, position) => {
    if (
      found == null &&
      position > start &&
      position < end &&
      node.type === schema.nodes.openQuestion &&
      node.attrs.questionId === questionId
    ) {
      found = { node, position };
      return false;
    }
    return found == null;
  });
  return found;
}

export function insertQuestionMarker(
  doc: ProseMirrorNode,
  sectionId: string,
  position: number,
  questionId: string,
  requestFingerprint: string,
): ProseMirrorNode {
  const section = findSection(doc, sectionId);
  if (
    !section ||
    position <= section.position ||
    position >= section.position + section.node.nodeSize
  ) {
    throw new Error(`The question anchor is outside spec section ${sectionId}.`);
  }
  const existing = findQuestionMarker(doc, questionId);
  if (existing) {
    if (
      existing.node.attrs.requestFingerprint === requestFingerprint &&
      existing.node.attrs.resolved !== true &&
      findQuestionMarker(doc, questionId, sectionId)
    ) {
      return doc;
    }
    throw new Error(`Open question ${questionId} belongs to a different request.`);
  }
  const marker = schema.nodes.openQuestion!.create({
    questionId,
    requestFingerprint,
    resolved: false,
    answerMarkdown: null,
  });
  return new Transform(doc).insert(position, marker).doc;
}

export function removeQuestionMarker(doc: ProseMirrorNode, questionId: string): ProseMirrorNode {
  const marker = findQuestionMarker(doc, questionId);
  if (!marker) return doc;
  return new Transform(doc).delete(marker.position, marker.position + marker.node.nodeSize).doc;
}

export function resolveQuestionMarker(
  doc: ProseMirrorNode,
  sectionId: string,
  questionId: string,
  answerMarkdown: string,
): ResolveQuestionMarkerResult {
  const marker = findQuestionMarker(doc, questionId, sectionId);
  if (!marker) throw new Error(`Open question ${questionId} has no document marker.`);
  if (marker.node.attrs.resolved === true) {
    if (marker.node.attrs.answerMarkdown === answerMarkdown) return { doc, replayed: true };
    throw new QuestionResolutionConflictError(questionId);
  }
  const resolvedMarker = schema.nodes.openQuestion!.create({
    ...marker.node.attrs,
    resolved: true,
    answerMarkdown,
  });
  const answerBlocks = parseMarkdownBlocks(answerMarkdown);
  const markerPosition = doc.resolve(marker.position);
  const parent = markerPosition.parent;
  if (!parent.isTextblock) {
    throw new Error(`Open question ${questionId} is not in a text block.`);
  }
  const parentStart = markerPosition.before(markerPosition.depth);
  const before = parent.content.cut(0, markerPosition.parentOffset);
  const after = parent.content.cut(markerPosition.parentOffset + marker.node.nodeSize);
  let replacement: ProseMirrorNode[];
  if (answerBlocks.length === 1 && answerBlocks[0]!.type === parent.type) {
    const answerContent = answerBlocks[0]!.content;
    const beforeSeparator = needsSpace(before, answerContent)
      ? Fragment.from(schema.text(" "))
      : Fragment.empty;
    const afterSeparator = needsSpace(answerContent, after)
      ? Fragment.from(schema.text(" "))
      : Fragment.empty;
    replacement = [
      parent.copy(
        before
          .append(beforeSeparator)
          .append(Fragment.from(resolvedMarker))
          .append(answerContent)
          .append(afterSeparator)
          .append(after),
      ),
    ];
  } else {
    const first = answerBlocks[0]!;
    const anchoredAnswer =
      first.isTextblock && !first.type.spec.code
        ? [
            first.copy(Fragment.from(resolvedMarker).append(first.content)),
            ...answerBlocks.slice(1),
          ]
        : [schema.nodes.paragraph!.create(null, resolvedMarker), ...answerBlocks];
    replacement = [
      ...(before.size > 0 ? [parent.copy(before)] : []),
      ...anchoredAnswer,
      ...(after.size > 0 ? [parent.copy(after)] : []),
    ];
  }
  const resolved = new Transform(doc).replaceWith(
    parentStart,
    parentStart + parent.nodeSize,
    replacement,
  ).doc;
  return { doc: resolved, replayed: false };
}

function needsSpace(left: Fragment, right: Fragment): boolean {
  if (left.size === 0 || right.size === 0) return false;
  const leftText = left.textBetween(0, left.size);
  const rightText = right.textBetween(0, right.size);
  return (
    leftText.length > 0 && rightText.length > 0 && !/\s$/.test(leftText) && !/^\s/.test(rightText)
  );
}
