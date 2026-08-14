import { useEffect, useMemo, useReducer } from "react";
import { SPEC_FRAGMENT_NAME, schema, type SectionState } from "@engrams/spec-document";
import type { Node as ProseMirrorNode } from "@tiptap/pm/model";
import { yXmlFragmentToProseMirrorRootNode } from "y-prosemirror";
import type * as Y from "yjs";

import type { SpecRail } from "@/hooks/useSpecRead";

export interface SpecSurfaceCredit {
  by: { id: string; name: string };
  at: string;
}

export interface SpecSurfaceSection {
  id: string;
  templateKey: string;
  title: string;
  state: SectionState;
  allowNa: boolean;
  naReason: string | null;
  isEmpty: boolean;
  isReached: boolean;
  isBeingRead: boolean;
  openQuestionCount: number;
  settledBy: { id: string; name: string } | null;
  stateChangedAt: string | null;
  credit: SpecSurfaceCredit | null;
  provenance: SpecSurfaceProvenance[];
}

export interface SpecSurface {
  sections: SpecSurfaceSection[];
  settledCount: number;
  totalCount: number;
  openQuestions: SpecSurfaceOpenQuestion[];
  provenanceRanges: SpecSurfaceProvenanceRange[];
  next: NextProposal;
}

export interface SpecSurfaceOpenQuestion {
  id: string;
  resolved: boolean;
  text: string | null;
}

export interface SpecSurfaceProvenance {
  label: string;
}

export interface SpecSurfaceProvenanceRange extends SpecSurfaceProvenance {
  from: number;
  to: number;
}

export interface SpecSurfaceOptions {
  readingSectionId?: string | null;
  openQuestions?: readonly { id: string; text: string }[];
}

const EMPTY_SPEC_SURFACE: SpecSurface = {
  sections: [],
  settledCount: 0,
  totalCount: 0,
  openQuestions: [],
  provenanceRanges: [],
  next: null,
};

/** Keep the shared surface current when either the rail or the Yjs document changes. */
export function useSpecSurface(
  rail: SpecRail | null | undefined,
  document: Y.Doc | null,
  options: SpecSurfaceOptions = {},
): SpecSurface {
  const [documentVersion, documentChanged] = useReducer((version: number) => version + 1, 0);
  const readingSectionId = options.readingSectionId;
  const openQuestions = options.openQuestions;
  useEffect(() => {
    if (document === null) return;
    document.on("update", documentChanged);
    return () => document.off("update", documentChanged);
  }, [document]);
  return useMemo(
    () =>
      rail
        ? deriveSpecSurface(rail, document, { readingSectionId, openQuestions })
        : EMPTY_SPEC_SURFACE,
    [document, documentVersion, openQuestions, rail, readingSectionId],
  );
}

export type NextProposal =
  | { kind: "draft_section"; sectionId: string; sectionTitle: string }
  | { kind: "settle_section"; sectionId: string; sectionTitle: string }
  | { kind: "look_for_breakage" }
  | null;

/** Return whether a section counts toward spec completion. */
export function isSectionComplete(section: { state: SectionState }): boolean {
  switch (section.state) {
    case "settled":
    case "n/a":
      return true;
    case "open":
    case "proposed":
      return false;
  }
}

/**
 * Derive the one surface model read by the section rail and document pane.
 * The rail owns workflow state. The live document contributes only facts that
 * cannot be known from the rail response.
 */
export function deriveSpecSurface(
  rail: SpecRail,
  document: Y.Doc | null,
  options: SpecSurfaceOptions = {},
): SpecSurface {
  const documentSections = document === null ? null : readDocumentSections(document);
  const questionText = new Map(
    options.openQuestions?.map((question) => [question.id, question.text]),
  );
  const readingSectionId = options.readingSectionId ?? rail.sections[0]?.id ?? null;
  const firstEmptyOpenIndex = rail.sections.findIndex((section) => {
    const documentSection = documentSections?.sections.get(section.id);
    return section.state === "open" && (documentSection?.isEmpty ?? false);
  });
  const surface: SpecSurface = {
    sections: rail.sections.map((section, index) => {
      const documentSection = documentSections?.sections.get(section.id);
      const isEmpty = documentSections === null ? false : (documentSection?.isEmpty ?? true);
      return {
        id: section.id,
        templateKey: section.templateKey,
        title: section.title,
        state: section.state,
        allowNa: section.allowNa,
        naReason: section.naReason,
        isEmpty,
        isReached:
          documentSections === null ||
          section.state !== "open" ||
          !isEmpty ||
          index === firstEmptyOpenIndex,
        isBeingRead: readingSectionId === section.id,
        openQuestionCount:
          documentSections === null
            ? section.openQuestionCount
            : (documentSection?.openQuestionCount ?? 0),
        settledBy: section.settledBy,
        stateChangedAt: section.stateChangedAt,
        credit:
          section.settledBy && section.stateChangedAt
            ? { by: section.settledBy, at: section.stateChangedAt }
            : null,
        provenance: documentSection?.provenance ?? [],
      };
    }),
    settledCount: rail.sections.filter((section) => section.state === "settled").length,
    totalCount: rail.sections.length,
    openQuestions:
      documentSections?.openQuestions.map((question) => ({
        ...question,
        text: questionText.get(question.id) ?? null,
      })) ?? [],
    provenanceRanges: documentSections?.provenanceRanges ?? [],
    next: null,
  };
  surface.next = deriveNextProposal(surface);
  return surface;
}

/** Return the one next offer in document order. Components own its wording. */
export function deriveNextProposal(surface: SpecSurface): NextProposal {
  if (surface.sections.length === 0) return null;

  const incomplete = surface.sections.filter((section) => !isSectionComplete(section));
  const open = incomplete.find((section) => section.state === "open");
  if (open) {
    return { kind: "draft_section", sectionId: open.id, sectionTitle: open.title };
  }
  const proposed = incomplete.find((section) => section.state === "proposed");
  if (proposed) {
    return { kind: "settle_section", sectionId: proposed.id, sectionTitle: proposed.title };
  }
  return { kind: "look_for_breakage" };
}

interface DocumentSectionFacts {
  isEmpty: boolean;
  openQuestionCount: number;
  provenance: SpecSurfaceProvenance[];
}

interface DocumentFacts {
  sections: Map<string, DocumentSectionFacts>;
  openQuestions: SpecSurfaceOpenQuestion[];
  provenanceRanges: SpecSurfaceProvenanceRange[];
}

const PROVENANCE_PATTERN = /[A-Za-z0-9_./-]+\s+@\s+[0-9a-f]{7,40}/gi;

function readDocumentSections(document: Y.Doc): DocumentFacts {
  const fragment = document.getXmlFragment(SPEC_FRAGMENT_NAME);
  if (fragment.length === 0) {
    return { sections: new Map(), openQuestions: [], provenanceRanges: [] };
  }
  const root = yXmlFragmentToProseMirrorRootNode(fragment, schema);
  const sections = new Map<string, DocumentSectionFacts>();
  const openQuestions: SpecSurfaceOpenQuestion[] = [];
  const provenanceRanges: SpecSurfaceProvenanceRange[] = [];
  root.forEach((section, sectionOffset) => {
    if (section.type.name !== "section" || typeof section.attrs.id !== "string") return;
    const facts = sectionFacts(section, sectionOffset, openQuestions, provenanceRanges);
    sections.set(section.attrs.id, facts);
  });
  return { sections, openQuestions, provenanceRanges };
}

function sectionFacts(
  section: ProseMirrorNode,
  sectionOffset: number,
  openQuestions: SpecSurfaceOpenQuestion[],
  provenanceRanges: SpecSurfaceProvenanceRange[],
): DocumentSectionFacts {
  let hasBody = false;
  let openQuestionCount = 0;
  const provenance: SpecSurfaceProvenance[] = [];
  section.forEach((block, _offset, index) => {
    if (index === 0) return;
    if (block.type.name === "diagramBlock") hasBody = true;
    block.descendants((node, nodeOffset) => {
      if (node.type.name === "openQuestion") {
        const resolved = node.attrs.resolved === true;
        openQuestions.push({ id: String(node.attrs.questionId), resolved, text: null });
        if (!resolved) openQuestionCount += 1;
        hasBody = true;
      } else if (node.isText && (node.text ?? "").trim().length > 0) {
        hasBody = true;
        for (const match of (node.text ?? "").matchAll(PROVENANCE_PATTERN)) {
          const label = match[0];
          if (!provenance.some((item) => item.label === label)) provenance.push({ label });
          const from = sectionOffset + 1 + _offset + 1 + nodeOffset + match.index!;
          provenanceRanges.push({ label, from, to: from + label.length });
        }
      }
      return true;
    });
  });
  return { isEmpty: !hasBody, openQuestionCount, provenance };
}
