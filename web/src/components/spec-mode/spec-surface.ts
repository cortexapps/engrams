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
  isEmpty: boolean;
  openQuestionCount: number;
  settledBy: { id: string; name: string } | null;
  stateChangedAt: string | null;
  credit: SpecSurfaceCredit | null;
}

export interface SpecSurface {
  sections: SpecSurfaceSection[];
  next: NextProposal;
}

export type NextProposal =
  | { kind: "draft_section"; sectionId: string; sectionTitle: string }
  | { kind: "settle_section"; sectionId: string; sectionTitle: string }
  | { kind: "look_for_breakage" }
  | null;

/**
 * Derive the one surface model read by the section rail and document pane.
 * The rail owns workflow state. The live document contributes only facts that
 * cannot be known from the rail response.
 */
export function deriveSpecSurface(rail: SpecRail, document: Y.Doc | null): SpecSurface {
  const documentSections = document === null ? null : readDocumentSections(document);
  const surface: SpecSurface = {
    sections: rail.sections.map((section) => {
      const documentSection = documentSections?.get(section.id);
      return {
        id: section.id,
        templateKey: section.templateKey,
        title: section.title,
        state: section.state,
        isEmpty: documentSections === null ? false : (documentSection?.isEmpty ?? true),
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
      };
    }),
    next: null,
  };
  surface.next = deriveNextProposal(surface);
  return surface;
}

/** Return the one next offer in document order. Components own its wording. */
export function deriveNextProposal(surface: SpecSurface): NextProposal {
  const open = surface.sections.find((section) => section.state === "open");
  if (open) {
    return { kind: "draft_section", sectionId: open.id, sectionTitle: open.title };
  }
  const proposed = surface.sections.find((section) => section.state === "proposed");
  if (proposed) {
    return { kind: "settle_section", sectionId: proposed.id, sectionTitle: proposed.title };
  }
  if (
    surface.sections.length > 0 &&
    surface.sections.every((section) => section.state === "settled")
  ) {
    return { kind: "look_for_breakage" };
  }
  return null;
}

interface DocumentSectionFacts {
  isEmpty: boolean;
  openQuestionCount: number;
}

function readDocumentSections(document: Y.Doc): Map<string, DocumentSectionFacts> {
  const fragment = document.getXmlFragment(SPEC_FRAGMENT_NAME);
  if (fragment.length === 0) return new Map();
  const root = yXmlFragmentToProseMirrorRootNode(fragment, schema);
  const result = new Map<string, DocumentSectionFacts>();
  root.forEach((section) => {
    if (section.type.name !== "section" || typeof section.attrs.id !== "string") return;
    result.set(section.attrs.id, sectionFacts(section));
  });
  return result;
}

function sectionFacts(section: ProseMirrorNode): DocumentSectionFacts {
  let hasBody = false;
  let openQuestionCount = 0;
  section.forEach((block, _offset, index) => {
    if (index === 0) return;
    if (block.type.name === "diagramBlock") hasBody = true;
    block.descendants((node) => {
      if (node.type.name === "openQuestion") {
        if (node.attrs.resolved !== true) openQuestionCount += 1;
        hasBody = true;
      } else if (node.isText && (node.text ?? "").trim().length > 0) {
        hasBody = true;
      }
      return true;
    });
  });
  return { isEmpty: !hasBody, openQuestionCount };
}
