import {
  absolutePositionToRelativePosition,
  initProseMirrorDoc,
  relativePositionToAbsolutePosition,
} from "y-prosemirror";
import * as Y from "yjs";

import { findSection, schema, SPEC_FRAGMENT_NAME } from "./schema.ts";

export interface SectionRelativeAnchor {
  sectionId: string;
  position: Uint8Array;
}

/** Create a Yjs relative position and bind it to a stable section node ID. */
export function createSectionRelativeAnchor(
  doc: Y.Doc,
  sectionId: string,
  absolutePosition: number,
): SectionRelativeAnchor {
  const fragment = doc.getXmlFragment(SPEC_FRAGMENT_NAME);
  const initialized = initProseMirrorDoc(fragment, schema);
  assertPositionInSection(initialized.doc, sectionId, absolutePosition);
  const relative = absolutePositionToRelativePosition(
    absolutePosition,
    fragment,
    initialized.mapping,
  );
  return { sectionId, position: Y.encodeRelativePosition(relative) };
}

/** Resolve an anchor after all current Yjs updates and verify its section ID. */
export function resolveSectionRelativeAnchor(
  doc: Y.Doc,
  anchor: SectionRelativeAnchor,
): number | null {
  const fragment = doc.getXmlFragment(SPEC_FRAGMENT_NAME);
  const initialized = initProseMirrorDoc(fragment, schema);
  const relative = Y.decodeRelativePosition(anchor.position);
  const position = relativePositionToAbsolutePosition(doc, fragment, relative, initialized.mapping);
  if (position == null) return null;
  try {
    assertPositionInSection(initialized.doc, anchor.sectionId, position);
    return position;
  } catch {
    return null;
  }
}

export function serializeSectionRelativeAnchor(anchor: SectionRelativeAnchor): string {
  return `yjs-section://${encodeURIComponent(anchor.sectionId)}/${hex(anchor.position)}`;
}

export function parseSectionRelativeAnchor(value: string): SectionRelativeAnchor {
  const match = /^yjs-section:\/\/([^/]+)\/([0-9a-f]+)$/.exec(value);
  if (!match || match[2]!.length % 2 !== 0) throw new Error("The section anchor is invalid.");
  const sectionId = decodeURIComponent(match[1]!);
  const position = new Uint8Array(match[2]!.length / 2);
  for (let index = 0; index < position.length; index += 1) {
    position[index] = Number.parseInt(match[2]!.slice(index * 2, index * 2 + 2), 16);
  }
  return { sectionId, position };
}

function assertPositionInSection(
  doc: ReturnType<typeof initProseMirrorDoc>["doc"],
  sectionId: string,
  position: number,
): void {
  const section = findSection(doc, sectionId);
  if (
    !section ||
    position <= section.position ||
    position >= section.position + section.node.nodeSize
  ) {
    throw new Error(`The anchor is outside spec section ${sectionId}.`);
  }
}

function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}
