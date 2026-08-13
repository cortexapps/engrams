/**
 * The talk-it-through stage: working notes (ADR 0114 D6, requirements R21-R23).
 *
 * The notes are the agent's live, correctable model of the conversation. They
 * are not the spec: they live in their own Yjs fragment inside the same
 * collaborative document, so every participant edits them over the ordinary
 * sync socket (R23), and no renderer, projection, export or checkpoint sees
 * them — those all read the spec fragment only.
 *
 * Bullets are structured tool output, never parsed prose. Each bullet carries a
 * verification mark: `verified` and `contradicted` must both name the receipt,
 * because a contradicted claim is only useful with the evidence beside it.
 *
 * `agentText` holds the text the agent believes. A person who edits a bullet
 * makes the two values differ, and `mergeWorkingNotes` reports that difference
 * back to the agent instead of overwriting it. This is the correction channel
 * that R23 asks for.
 */

import { Node as ProseMirrorNode, Schema, type NodeSpec } from "prosemirror-model";

/** The Yjs fragment that holds the notes, beside the spec fragment. */
export const SPEC_NOTES_FRAGMENT_NAME = "spec-notes";
/** The Yjs map that holds the stage state (the archive stamp). */
export const SPEC_NOTES_STATE_NAME = "spec-notes-state";
/** The key in that map. An ISO timestamp archives the notes. */
export const SPEC_NOTES_ARCHIVED_AT_KEY = "archivedAt";

export const SPEC_NOTES_MAX_CLUSTERS = 40;
export const SPEC_NOTES_MAX_BULLETS_PER_CLUSTER = 40;
export const SPEC_NOTES_MAX_SECTION_TAGS = 4;
export const SPEC_NOTE_TEXT_MAX_LENGTH = 2_000;
export const SPEC_NOTE_THEME_MAX_LENGTH = 200;
export const SPEC_NOTE_PROVENANCE_MAX_LENGTH = 500;

export const SPEC_NOTE_MARKS = ["verified", "contradicted", "unchecked"] as const;
export type SpecNoteMark = (typeof SPEC_NOTE_MARKS)[number];

export const SPEC_NOTE_KINDS = ["observation", "requirement", "tension", "question"] as const;
export type SpecNoteKind = (typeof SPEC_NOTE_KINDS)[number];

/** The mark as a person reads it on the canvas. */
export const SPEC_NOTE_MARK_GLYPHS: Readonly<Record<SpecNoteMark, string>> = {
  verified: "✓",
  contradicted: "✗",
  unchecked: "?",
};

/** The theme of the pile that carries no destination tag (R22). */
export const SPEC_NOTES_UNTAGGED_THEME = "untagged";
const SPEC_NOTES_UNTAGGED_CLUSTER_ID = "untagged";

/** A marker that the spec parser reads. Notes text may not smuggle one in. */
const OPEN_QUESTION_MARKER = "{{open-question:";

export interface SpecNoteBullet {
  /** Stable inside one notes set. The agent chooses it and reuses it. */
  id: string;
  mark: SpecNoteMark;
  kind: SpecNoteKind;
  text: string;
  /** The receipt. Required for a verified or contradicted bullet. */
  provenance: string | null;
  /** The text the agent believes. It differs when a person corrected it. */
  agentText: string;
}

export interface SpecNoteCluster {
  id: string;
  theme: string;
  /** Destination sections. Empty means the bullet sits in the untagged pile. */
  sectionIds: string[];
  bullets: SpecNoteBullet[];
}

export interface SpecWorkingNotes {
  clusters: SpecNoteCluster[];
}

/** What the agent sends. `agentText` is derived, so the tool never carries it. */
export interface SpecNoteBulletInput {
  id: string;
  mark: SpecNoteMark;
  kind: SpecNoteKind;
  text: string;
  provenance?: string | null;
}

export interface SpecNoteClusterInput {
  id: string;
  theme: string;
  sectionIds?: string[];
  bullets: SpecNoteBulletInput[];
}

export interface SpecWorkingNotesInput {
  clusters: SpecNoteClusterInput[];
}

export class SpecWorkingNotesError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SpecWorkingNotesError";
  }
}

export const specNotesNodeSpecs: Readonly<Record<string, NodeSpec>> = {
  doc: { content: "noteCluster+" },
  noteCluster: {
    content: "noteBullet+",
    // Every attribute carries a default because ProseMirror only generates a
    // node in a required content position when it can create it unattended.
    // Identity is enforced by validateWorkingNotes, exactly as it is for a
    // spec section.
    //
    // The theme and the destination tags are attributes, not editable content:
    // they are the agent's clustering. A person edits the words in a bullet.
    attrs: {
      id: { default: null },
      theme: { default: "" },
      sectionIds: { default: [] },
    },
    isolating: true,
  },
  noteBullet: {
    content: "inline*",
    marks: "",
    attrs: {
      id: { default: null },
      mark: { default: "unchecked" },
      kind: { default: "observation" },
      provenance: { default: null },
      agentText: { default: "" },
    },
  },
  text: { group: "inline" },
};

export const notesSchema = new Schema({ nodes: specNotesNodeSpecs });

export function isSpecNoteMark(value: unknown): value is SpecNoteMark {
  return SPEC_NOTE_MARKS.includes(value as SpecNoteMark);
}

export function isSpecNoteKind(value: unknown): value is SpecNoteKind {
  return SPEC_NOTE_KINDS.includes(value as SpecNoteKind);
}

/**
 * Enforce the notes contract before anything is stored. The shape is what the
 * canvas renders and what distillation reads, so a bad set fails at the tool
 * and at the sync socket, never in the browser.
 */
export function validateWorkingNotes(notes: SpecWorkingNotes): void {
  if (notes.clusters.length === 0) {
    throw new SpecWorkingNotesError("Working notes need at least one cluster.");
  }
  if (notes.clusters.length > SPEC_NOTES_MAX_CLUSTERS) {
    throw new SpecWorkingNotesError(
      `Working notes hold at most ${SPEC_NOTES_MAX_CLUSTERS} clusters.`,
    );
  }
  const clusterIds = new Set<string>();
  const bulletIds = new Set<string>();
  for (const cluster of notes.clusters) {
    requireIdentifier(cluster.id, "A note cluster");
    if (clusterIds.has(cluster.id)) {
      throw new SpecWorkingNotesError(`Note cluster ${cluster.id} is used twice.`);
    }
    clusterIds.add(cluster.id);
    requireLine(cluster.theme, SPEC_NOTE_THEME_MAX_LENGTH, `Cluster ${cluster.id} theme`);
    if (cluster.sectionIds.length > SPEC_NOTES_MAX_SECTION_TAGS) {
      throw new SpecWorkingNotesError(
        `Cluster ${cluster.id} carries more than ${SPEC_NOTES_MAX_SECTION_TAGS} section tags.`,
      );
    }
    const tags = new Set<string>();
    for (const sectionId of cluster.sectionIds) {
      requireIdentifier(sectionId, `A section tag on cluster ${cluster.id}`);
      if (tags.has(sectionId)) {
        throw new SpecWorkingNotesError(`Cluster ${cluster.id} tags ${sectionId} twice.`);
      }
      tags.add(sectionId);
    }
    if (cluster.bullets.length === 0) {
      throw new SpecWorkingNotesError(`Cluster ${cluster.id} needs at least one bullet.`);
    }
    if (cluster.bullets.length > SPEC_NOTES_MAX_BULLETS_PER_CLUSTER) {
      throw new SpecWorkingNotesError(
        `Cluster ${cluster.id} holds more than ${SPEC_NOTES_MAX_BULLETS_PER_CLUSTER} bullets.`,
      );
    }
    for (const bullet of cluster.bullets) {
      requireIdentifier(bullet.id, "A note bullet");
      if (bulletIds.has(bullet.id)) {
        throw new SpecWorkingNotesError(`Note bullet ${bullet.id} is used twice.`);
      }
      bulletIds.add(bullet.id);
      if (!isSpecNoteMark(bullet.mark)) {
        throw new SpecWorkingNotesError(
          `Bullet ${bullet.id} needs a mark: ${SPEC_NOTE_MARKS.join(", ")}.`,
        );
      }
      if (!isSpecNoteKind(bullet.kind)) {
        throw new SpecWorkingNotesError(
          `Bullet ${bullet.id} needs a kind: ${SPEC_NOTE_KINDS.join(", ")}.`,
        );
      }
      requireLine(bullet.text, SPEC_NOTE_TEXT_MAX_LENGTH, `Bullet ${bullet.id} text`);
      requireLine(bullet.agentText, SPEC_NOTE_TEXT_MAX_LENGTH, `Bullet ${bullet.id} agent text`);
      const provenance = bullet.provenance;
      if (bullet.mark !== "unchecked") {
        if (provenance === null || provenance.trim().length === 0) {
          throw new SpecWorkingNotesError(
            `Bullet ${bullet.id} is ${bullet.mark}, so it must name the receipt in its provenance.`,
          );
        }
      }
      if (provenance !== null) {
        requireLine(provenance, SPEC_NOTE_PROVENANCE_MAX_LENGTH, `Bullet ${bullet.id} provenance`);
      }
    }
  }
}

/** Read the agent's input as a full notes set. Every bullet starts unedited. */
export function workingNotesFromInput(input: SpecWorkingNotesInput): SpecWorkingNotes {
  return {
    clusters: input.clusters.map((cluster) => ({
      id: cluster.id,
      theme: cluster.theme,
      sectionIds: [...(cluster.sectionIds ?? [])],
      bullets: cluster.bullets.map((bullet) => ({
        id: bullet.id,
        mark: bullet.mark,
        kind: bullet.kind,
        text: bullet.text,
        provenance: bullet.provenance ?? null,
        agentText: bullet.text,
      })),
    })),
  };
}

export interface SpecNoteCorrection {
  bulletId: string;
  /** What the agent last wrote. */
  agentText: string;
  /** What the person made it say. This is the text the notes keep. */
  personText: string;
  /** True when the agent dropped a bullet that a person had corrected. */
  keptAgainstDrop: boolean;
}

export interface MergedWorkingNotes {
  notes: SpecWorkingNotes;
  corrections: SpecNoteCorrection[];
}

/**
 * Merge the agent's replacement over the notes a person may have edited.
 *
 * A bullet whose current text left `agentText` behind was corrected by hand.
 * The correction wins, it is reported to the agent, and `agentText` moves to
 * the corrected wording so the same correction is reported exactly once. A
 * corrected bullet the agent leaves out survives the replace: it moves to the
 * untagged pile when its cluster is gone, because deleting a person's words to
 * make room for the agent's model is the failure R23 exists to stop.
 */
export function mergeWorkingNotes(
  current: SpecWorkingNotes | null,
  proposed: SpecWorkingNotes,
): MergedWorkingNotes {
  const corrections: SpecNoteCorrection[] = [];
  const corrected = new Map<string, { cluster: SpecNoteCluster; bullet: SpecNoteBullet }>();
  for (const cluster of current?.clusters ?? []) {
    for (const bullet of cluster.bullets) {
      if (bullet.text !== bullet.agentText) corrected.set(bullet.id, { cluster, bullet });
    }
  }

  const proposedIds = new Set(
    proposed.clusters.flatMap((cluster) => cluster.bullets.map((bullet) => bullet.id)),
  );
  const clusters = proposed.clusters.map((cluster) => ({
    ...cluster,
    sectionIds: [...cluster.sectionIds],
    bullets: cluster.bullets.map((bullet) => {
      const edit = corrected.get(bullet.id);
      if (!edit) return { ...bullet };
      corrections.push({
        bulletId: bullet.id,
        agentText: edit.bullet.agentText,
        personText: edit.bullet.text,
        keptAgainstDrop: false,
      });
      return { ...bullet, text: edit.bullet.text, agentText: edit.bullet.text };
    }),
  }));

  const rescued: SpecNoteBullet[] = [];
  for (const [bulletId, edit] of corrected) {
    if (proposedIds.has(bulletId)) continue;
    corrections.push({
      bulletId,
      agentText: edit.bullet.agentText,
      personText: edit.bullet.text,
      keptAgainstDrop: true,
    });
    const kept = { ...edit.bullet, agentText: edit.bullet.text };
    const home = clusters.find((cluster) => cluster.id === edit.cluster.id);
    if (home) home.bullets.push(kept);
    else rescued.push(kept);
  }
  if (rescued.length > 0) {
    const pile = clusters.find((cluster) => cluster.sectionIds.length === 0);
    if (pile) pile.bullets.push(...rescued);
    else
      clusters.push({
        id: SPEC_NOTES_UNTAGGED_CLUSTER_ID,
        theme: SPEC_NOTES_UNTAGGED_THEME,
        sectionIds: [],
        bullets: rescued,
      });
  }
  return { notes: { clusters }, corrections };
}

/** R22's readiness gauge: bullets that carry no destination section yet. */
export function untaggedBulletCount(notes: SpecWorkingNotes): number {
  return notes.clusters
    .filter((cluster) => cluster.sectionIds.length === 0)
    .reduce((total, cluster) => total + cluster.bullets.length, 0);
}

export function buildWorkingNotesDocument(notes: SpecWorkingNotes): ProseMirrorNode {
  validateWorkingNotes(notes);
  return notesSchema.nodes.doc!.create(
    null,
    notes.clusters.map((cluster) =>
      notesSchema.nodes.noteCluster!.create(
        { id: cluster.id, theme: cluster.theme, sectionIds: cluster.sectionIds },
        cluster.bullets.map((bullet) =>
          notesSchema.nodes.noteBullet!.create(
            {
              id: bullet.id,
              mark: bullet.mark,
              kind: bullet.kind,
              provenance: bullet.provenance,
              agentText: bullet.agentText,
            },
            notesSchema.text(bullet.text),
          ),
        ),
      ),
    ),
  );
}

export function readWorkingNotes(document: ProseMirrorNode): SpecWorkingNotes {
  const clusters: SpecNoteCluster[] = [];
  // Node types are compared by name: the browser editor builds an equivalent
  // schema of its own, and it reads the notes with this same function.
  document.forEach((cluster) => {
    if (cluster.type.name !== "noteCluster") {
      throw new SpecWorkingNotesError("Working notes can contain only clusters.");
    }
    const bullets: SpecNoteBullet[] = [];
    cluster.forEach((child) => {
      bullets.push({
        id: stringAttribute(child.attrs.id, "A note bullet needs an id."),
        mark: isSpecNoteMark(child.attrs.mark) ? child.attrs.mark : "unchecked",
        kind: isSpecNoteKind(child.attrs.kind) ? child.attrs.kind : "observation",
        text: child.textContent,
        provenance: typeof child.attrs.provenance === "string" ? child.attrs.provenance : null,
        agentText: typeof child.attrs.agentText === "string" ? child.attrs.agentText : "",
      });
    });
    clusters.push({
      id: stringAttribute(cluster.attrs.id, "A note cluster needs an id."),
      theme: typeof cluster.attrs.theme === "string" ? cluster.attrs.theme : "",
      sectionIds: sectionTags(cluster.attrs.sectionIds),
      bullets,
    });
  });
  return { clusters };
}

export interface DistilledSection {
  sectionId: string;
  /** Markdown blocks appended to the section body. */
  markdown: string;
}

export interface WorkingNotesDistillation {
  sections: DistilledSection[];
  /** Contradicted bullets, which never enter the spec. */
  refutedBullets: number;
  /** Bullets still in the untagged pile when the stage closed. */
  untaggedBullets: number;
}

/**
 * Convert the tagged clusters into section material (R22).
 *
 * A contradicted bullet is a refuted claim, so it stays in the archive and
 * never becomes spec content. Nothing is invented to fill a template: a
 * destination that no cluster tags, and a destination whose clusters hold only
 * refuted claims, produce no material at all and leave the section empty.
 */
export function distillWorkingNotes(notes: SpecWorkingNotes): WorkingNotesDistillation {
  const bySection = new Map<string, string[]>();
  let refutedBullets = 0;
  for (const cluster of notes.clusters) {
    const lines = cluster.bullets
      .filter((bullet) => {
        if (bullet.mark !== "contradicted") return true;
        refutedBullets += 1;
        return false;
      })
      .map(distilledLine);
    if (lines.length === 0) continue;
    for (const sectionId of cluster.sectionIds) {
      const blocks = bySection.get(sectionId) ?? [];
      blocks.push(`### ${cluster.theme}`, lines.join("\n\n"));
      bySection.set(sectionId, blocks);
    }
  }
  return {
    sections: [...bySection].map(([sectionId, blocks]) => ({
      sectionId,
      markdown: `${blocks.join("\n\n")}\n`,
    })),
    refutedBullets,
    untaggedBullets: untaggedBulletCount(notes),
  };
}

/**
 * One bullet as one paragraph.
 *
 * Every line opens with its label, so a distilled paragraph can never start
 * with markdown that the section parser would read as structure.
 */
function distilledLine(bullet: SpecNoteBullet): string {
  const receipt = bullet.provenance === null ? "" : ` (${bullet.provenance})`;
  return `${distilledLabel(bullet)} — ${bullet.text}${receipt}`;
}

function distilledLabel(bullet: SpecNoteBullet): string {
  if (bullet.kind === "requirement") return "Requirement candidate";
  if (bullet.kind === "tension") return "Tension";
  if (bullet.kind === "question") return "Open question";
  return bullet.mark === "verified" ? "Verified" : "Unverified";
}

function requireIdentifier(value: unknown, subject: string): void {
  if (typeof value !== "string" || value.trim().length === 0 || value.length > 200) {
    throw new SpecWorkingNotesError(`${subject} needs an id of 1 to 200 characters.`);
  }
}

function requireLine(value: string, maxLength: number, subject: string): void {
  if (typeof value !== "string" || value.trim().length === 0) {
    throw new SpecWorkingNotesError(`${subject} cannot be empty.`);
  }
  if (value.length > maxLength) {
    throw new SpecWorkingNotesError(`${subject} is longer than ${maxLength} characters.`);
  }
  if (/[\r\n]/.test(value)) {
    throw new SpecWorkingNotesError(`${subject} must be one line.`);
  }
  if (value.includes(OPEN_QUESTION_MARKER)) {
    throw new SpecWorkingNotesError(`${subject} cannot contain an open-question marker.`);
  }
}

function stringAttribute(value: unknown, message: string): string {
  if (typeof value !== "string" || value.trim().length === 0) {
    throw new SpecWorkingNotesError(message);
  }
  return value;
}

function sectionTags(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  return value.filter((entry): entry is string => typeof entry === "string");
}
