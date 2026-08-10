import { describe, expect, test } from "bun:test";
import { prosemirrorToYDoc, yXmlFragmentToProseMirrorRootNode } from "y-prosemirror";
import * as Y from "yjs";

import {
  createSectionRelativeAnchor,
  parseSectionRelativeAnchor,
  resolveSectionRelativeAnchor,
  serializeSectionRelativeAnchor,
} from "./anchors.ts";
import { schema, SPEC_FRAGMENT_NAME } from "./schema.ts";

function documentWithText() {
  const text = schema.text("alpha beta");
  const section = schema.nodes.section!.create(
    { id: "requirements", templateSectionKey: "requirements" },
    [
      schema.nodes.sectionHeading!.create(null, schema.text("Requirements")),
      schema.nodes.paragraph!.create(null, text),
    ],
  );
  return schema.nodes.doc!.create(null, section);
}

function textPosition(doc: ReturnType<typeof documentWithText>, value: string): number {
  let found: number | null = null;
  doc.descendants((node, position) => {
    if (found == null && node.isText && node.text === value) found = position;
  });
  if (found == null) throw new Error("The test text does not exist.");
  return found;
}

function yText(type: Y.AbstractType<unknown>): Y.XmlText | null {
  if (type instanceof Y.XmlText && type.toString() === "alpha beta") return type;
  if (type instanceof Y.XmlFragment || type instanceof Y.XmlElement) {
    for (const child of type.toArray()) {
      const found = yText(child);
      if (found) return found;
    }
  }
  return null;
}

describe("section relative anchors", () => {
  test("survive a concurrent edit in the same section", () => {
    const base = prosemirrorToYDoc(documentWithText(), SPEC_FRAGMENT_NAME);
    const replica = new Y.Doc();
    Y.applyUpdate(replica, Y.encodeStateAsUpdate(base));
    const beforeReplicaEdit = Y.encodeStateVector(replica);
    const initial = yXmlFragmentToProseMirrorRootNode(
      base.getXmlFragment(SPEC_FRAGMENT_NAME),
      schema,
    );
    const anchor = createSectionRelativeAnchor(
      base,
      "requirements",
      textPosition(initial, "alpha beta") + "alpha ".length,
    );

    const text = yText(replica.getXmlFragment(SPEC_FRAGMENT_NAME));
    if (!text) throw new Error("The Yjs text does not exist.");
    text.insert(0, "concurrent ");
    Y.applyUpdate(base, Y.encodeStateAsUpdate(replica, beforeReplicaEdit));

    const resolved = resolveSectionRelativeAnchor(base, anchor);
    expect(resolved).not.toBeNull();
    const merged = yXmlFragmentToProseMirrorRootNode(
      base.getXmlFragment(SPEC_FRAGMENT_NAME),
      schema,
    );
    expect(merged.textBetween(resolved!, resolved! + 4)).toBe("beta");
  });

  test("serializes without losing the section binding", () => {
    const doc = prosemirrorToYDoc(documentWithText(), SPEC_FRAGMENT_NAME);
    const proseMirror = yXmlFragmentToProseMirrorRootNode(
      doc.getXmlFragment(SPEC_FRAGMENT_NAME),
      schema,
    );
    const anchor = createSectionRelativeAnchor(
      doc,
      "requirements",
      textPosition(proseMirror, "alpha beta") + 1,
    );
    const parsed = parseSectionRelativeAnchor(serializeSectionRelativeAnchor(anchor));
    expect(parsed.sectionId).toBe("requirements");
    expect(parsed.position).toEqual(anchor.position);
  });
});
