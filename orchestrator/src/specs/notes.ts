/**
 * The talk-it-through stage (ADR 0114 D6, requirements R21-R23).
 *
 * Working notes ride the collaborative document, in their own Yjs fragment
 * beside the spec. That is what makes a bullet editable by every participant
 * over the ordinary sync socket (R23), and it is why this stage needs no table
 * of its own: the notes are durable in the update log the document already has.
 *
 * A notes write is cache-only, so the spec revision never moves while the pen
 * is down. Distillation is the one semantic write: it appends the tagged
 * clusters to their destination sections and stamps the archive in a single
 * Yjs update, so a crash cannot leave the notes live beside distilled sections
 * or duplicate the material on a replay.
 */

import {
  distillWorkingNotes,
  findSection,
  mergeWorkingNotes,
  parseMarkdownBlocks,
  replaceSection,
  schema,
  SPEC_NOTES_ARCHIVED_AT_KEY,
  SPEC_NOTES_FRAGMENT_NAME,
  SPEC_NOTES_STATE_NAME,
  SpecWorkingNotesError,
  untaggedBulletCount,
  validateWorkingNotes,
  workingNotesFromInput,
  buildWorkingNotesDocument,
  type SpecNoteCorrection,
  type SpecWorkingNotes,
  type SpecWorkingNotesInput,
  type WorkingNotesDistillation,
} from "@engrams/spec-document";
import type { Node as ProseMirrorNode } from "prosemirror-model";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import type * as Y from "yjs";

import {
  proseMirrorDocument,
  readSpecWorkingNotes,
  specNotesArchivedAt,
  SpecNotesArchivedError,
  type SpecDocumentService,
} from "./doc-service.ts";

/** What the canvas renders and what the agent reads back. */
export interface SpecWorkingNotesStage {
  notes: SpecWorkingNotes;
  /** An ISO stamp once the stage closed. The archive is read-only (R23). */
  archivedAt: string | null;
  /** R22's readiness gauge. */
  untaggedBullets: number;
}

export interface UpdateWorkingNotesInput {
  specId: string;
  notes: SpecWorkingNotesInput;
  clientId: string;
  expectedRev?: bigint;
}

export interface UpdateWorkingNotesResult {
  stage: SpecWorkingNotesStage;
  /** Bullets a person rewrote since the agent's last write (R23). */
  corrections: SpecNoteCorrection[];
  newRev: bigint;
}

export interface DistillWorkingNotesInput {
  specId: string;
  clientId: string;
  expectedRev?: bigint;
}

export interface DistillWorkingNotesResult {
  distillation: WorkingNotesDistillation;
  stage: SpecWorkingNotesStage;
  /** False when the notes were already archived by an earlier call. */
  applied: boolean;
  newRev: bigint;
}

export interface SpecWorkingNotesServiceOptions {
  documents: Pick<SpecDocumentService, "syncFromLog" | "mutateNotes" | "mutateDocument">;
  now: () => Date;
}

export class SpecWorkingNotesService {
  constructor(private readonly options: SpecWorkingNotesServiceOptions) {}

  async readStage(specId: string): Promise<SpecWorkingNotesStage | null> {
    const loaded = await this.options.documents.syncFromLog(specId);
    return stageOf(loaded.doc);
  }

  /**
   * Replace the notes with the agent's current model of the conversation.
   *
   * The replacement is merged over the notes, so a bullet a person rewrote
   * keeps its words and is reported back instead of being overwritten (R23).
   */
  async update(input: UpdateWorkingNotesInput): Promise<UpdateWorkingNotesResult> {
    const loaded = await this.options.documents.syncFromLog(input.specId);
    const archivedAt = specNotesArchivedAt(loaded.doc);
    if (archivedAt !== null) throw new SpecNotesArchivedError(archivedAt);
    const document = proseMirrorDocument(loaded.doc);
    const proposed = workingNotesFromInput(input.notes);
    validateWorkingNotes(proposed);
    requireKnownSections(document, proposed);
    const current = readSpecWorkingNotes(loaded.doc);
    const merged = mergeWorkingNotes(current, proposed);
    validateWorkingNotes(merged.notes);
    const replacement = buildWorkingNotesDocument(merged.notes);
    if (current !== null && sameNotes(current, merged.notes)) {
      return {
        stage: stageValue(merged.notes, null),
        corrections: merged.corrections,
        newRev: loaded.semanticDocSeq,
      };
    }
    const update = await this.options.documents.mutateNotes(
      input.specId,
      input.clientId,
      (ydoc) => writeNotesFragment(ydoc, replacement),
      input.expectedRev,
    );
    return {
      stage: stageValue(merged.notes, null),
      corrections: merged.corrections,
      newRev: update.semanticDocSeq,
    };
  }

  /**
   * Close the stage: convert the tagged clusters into section material and
   * archive the notes (R22, R23).
   *
   * A destination with no tagged material stays empty. Nothing is invented to
   * fill a template, and a refuted claim never becomes spec content.
   */
  async distill(input: DistillWorkingNotesInput): Promise<DistillWorkingNotesResult> {
    const loaded = await this.options.documents.syncFromLog(input.specId);
    const notes = readSpecWorkingNotes(loaded.doc);
    if (notes === null) {
      throw new SpecWorkingNotesError("This spec has no working notes to distil.");
    }
    const distillation = distillWorkingNotes(notes);
    const archivedAt = specNotesArchivedAt(loaded.doc);
    if (archivedAt !== null) {
      return {
        distillation,
        stage: stageValue(notes, archivedAt),
        applied: false,
        newRev: loaded.semanticDocSeq,
      };
    }
    for (const section of distillation.sections) {
      requireSection(proseMirrorDocument(loaded.doc), section.sectionId);
    }
    const stamp = this.options.now().toISOString();
    const update = await this.options.documents.mutateDocument(
      input.specId,
      input.clientId,
      (document, ydoc) => {
        ydoc.getMap(SPEC_NOTES_STATE_NAME).set(SPEC_NOTES_ARCHIVED_AT_KEY, stamp);
        return distillation.sections.reduce(
          (next, section) => appendSectionBlocks(next, section.sectionId, section.markdown),
          document,
        );
      },
      input.expectedRev,
    );
    return {
      distillation,
      stage: stageValue(notes, stamp),
      applied: true,
      newRev: update.semanticDocSeq,
    };
  }
}

function stageOf(doc: Y.Doc): SpecWorkingNotesStage | null {
  const notes = readSpecWorkingNotes(doc);
  if (notes === null) return null;
  return stageValue(notes, specNotesArchivedAt(doc));
}

function stageValue(notes: SpecWorkingNotes, archivedAt: string | null): SpecWorkingNotesStage {
  return { notes, archivedAt, untaggedBullets: untaggedBulletCount(notes) };
}

function sameNotes(left: SpecWorkingNotes, right: SpecWorkingNotes): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function writeNotesFragment(ydoc: Y.Doc, replacement: ProseMirrorNode): void {
  prosemirrorToYXmlFragment(replacement, ydoc.getXmlFragment(SPEC_NOTES_FRAGMENT_NAME));
}

/** A destination tag must name a section this template really has. */
function requireKnownSections(document: ProseMirrorNode, notes: SpecWorkingNotes): void {
  for (const cluster of notes.clusters) {
    for (const sectionId of cluster.sectionIds) {
      if (!findSection(document, sectionId)) {
        throw new SpecWorkingNotesError(
          `Cluster ${cluster.id} is tagged toward an unknown section: ${sectionId}.`,
        );
      }
    }
  }
}

function requireSection(document: ProseMirrorNode, sectionId: string): ProseMirrorNode {
  const found = findSection(document, sectionId);
  if (!found) throw new SpecWorkingNotesError(`Unknown spec section: ${sectionId}`);
  return found.node;
}

/**
 * Append the distilled material to a section body.
 *
 * The stage keeps the pen down, so a destination is normally empty and the
 * placeholder paragraph is replaced. A section that already holds prose keeps
 * it: distillation adds material, it never overwrites an author.
 */
function appendSectionBlocks(
  document: ProseMirrorNode,
  sectionId: string,
  markdown: string,
): ProseMirrorNode {
  const section = requireSection(document, sectionId);
  const heading = section.firstChild;
  if (!heading || heading.type !== schema.nodes.sectionHeading) {
    throw new SpecWorkingNotesError(`Spec section ${sectionId} has no stable heading.`);
  }
  const body: ProseMirrorNode[] = [];
  section.forEach((node, _offset, index) => {
    if (index > 0) body.push(node);
  });
  const kept = isEmptyBody(body) ? [] : body;
  return replaceSection(
    document,
    sectionId,
    section.type.create(section.attrs, [heading, ...kept, ...parseMarkdownBlocks(markdown)]),
  );
}

function isEmptyBody(body: readonly ProseMirrorNode[]): boolean {
  return body.every((node) => node.type === schema.nodes.paragraph && node.content.size === 0);
}
