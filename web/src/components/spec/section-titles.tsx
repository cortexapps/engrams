import { createContext, useContext, type ReactNode } from "react";
import type { Node as ProseMirrorNode } from "@tiptap/pm/model";

/**
 * Section titles, read from the live document.
 *
 * A destination tag on a note cluster names a section id. The title beside it
 * comes from the same document the tag points into, so a retitled heading
 * renames the tag with no extra fetch (ADR 0114 D2 keeps the identity stable).
 */
const SpecSectionTitles = createContext<ReadonlyMap<string, string>>(new Map());

export function SpecSectionTitleProvider({
  titles,
  children,
}: {
  titles: ReadonlyMap<string, string>;
  children: ReactNode;
}) {
  return <SpecSectionTitles.Provider value={titles}>{children}</SpecSectionTitles.Provider>;
}

export function useSpecSectionTitles(): ReadonlyMap<string, string> {
  return useContext(SpecSectionTitles);
}

export function readSectionTitles(doc: ProseMirrorNode): Map<string, string> {
  const titles = new Map<string, string>();
  doc.forEach((section) => {
    if (section.type.name !== "section" || typeof section.attrs.id !== "string") return;
    const heading = section.firstChild;
    titles.set(section.attrs.id, heading?.textContent || section.attrs.id);
  });
  return titles;
}
