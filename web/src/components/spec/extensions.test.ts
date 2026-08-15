import { describe, expect, test } from "vitest";
import { createTemplateDocument, schema, specNodeSpecs } from "@engrams/spec-document";
import { EditorState } from "@tiptap/pm/state";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import * as Y from "yjs";

import { createSectionStructurePlugin, hasSameSectionStructure, sectionIds } from "./extensions";

const template = {
  sections: [
    { id: "context", key: "context", title: "Context" },
    { id: "failure-modes", key: "failure-modes", title: "Failure modes" },
  ],
};

describe("the spec canvas schema", () => {
  test("allows the server document to replace Tiptap's initial placeholder section", () => {
    const placeholder = schema.nodes.doc!.create(null, [
      schema.nodes.section!.create(null, [
        schema.nodes.sectionHeading!.create(),
        schema.nodes.paragraph!.create(),
      ]),
    ]);
    const state = EditorState.create({
      doc: placeholder,
      plugins: [createSectionStructurePlugin()],
    });
    const serverDocument = createTemplateDocument(template);
    const result = state.applyTransaction(
      state.tr.replaceWith(0, placeholder.content.size, serverDocument.content),
    );

    expect(result.transactions).toHaveLength(1);
    expect(sectionIds(result.state.doc)).toEqual(["context", "failure-modes"]);
  });

  test("refuses a transaction that deletes a structural section", () => {
    const doc = createTemplateDocument(template);
    const state = EditorState.create({ doc, plugins: [createSectionStructurePlugin()] });
    const second = doc.child(1);
    const deleteSecond = state.tr.delete(
      doc.child(0).nodeSize,
      doc.child(0).nodeSize + second.nodeSize,
    );
    const result = state.applyTransaction(deleteSecond);

    expect(result.transactions).toHaveLength(0);
    expect(sectionIds(result.state.doc)).toEqual(["context", "failure-modes"]);
  });

  test("allows content edits that keep the shared section identities", () => {
    const doc = createTemplateDocument(template);
    const textPosition = 2;
    const changed = EditorState.create({ schema, doc }).tr.insertText("Updated ", textPosition).doc;

    expect(hasSameSectionStructure(doc, changed)).toBe(true);
    expect(Object.keys(specNodeSpecs)).toEqual([
      "doc",
      "section",
      "sectionHeading",
      "paragraph",
      "heading",
      "bulletList",
      "orderedList",
      "listItem",
      "codeBlock",
      "diagramBlock",
      "openQuestion",
      "text",
    ]);
  });

  test("keeps an anchor stable during a concurrent edit in another section", () => {
    const source = new Y.Doc();
    prosemirrorToYXmlFragment(
      createTemplateDocument(template),
      source.getXmlFragment("prosemirror"),
    );
    const initial = Y.encodeStateAsUpdate(source);
    const first = new Y.Doc();
    const second = new Y.Doc();
    Y.applyUpdate(first, initial);
    Y.applyUpdate(second, initial);

    const firstFragment = first.getXmlFragment("prosemirror");
    const anchoredSection = firstFragment.get(0);
    if (!(anchoredSection instanceof Y.XmlElement)) throw new Error("expected the first section");
    const anchor = Y.createRelativePositionFromTypeIndex(anchoredSection, 0);
    const firstVector = Y.encodeStateVector(first);
    const secondVector = Y.encodeStateVector(second);

    insertTextInSection(first, 0, "Human A");
    insertTextInSection(second, 1, "Human B");
    Y.applyUpdate(first, Y.encodeStateAsUpdate(second, firstVector));
    Y.applyUpdate(second, Y.encodeStateAsUpdate(first, secondVector));

    const resolved = Y.createAbsolutePositionFromRelativePosition(anchor, first);
    expect(resolved?.type).toBe(first.getXmlFragment("prosemirror").get(0));
    expect(Y.encodeStateVector(first)).toEqual(Y.encodeStateVector(second));
    source.destroy();
    first.destroy();
    second.destroy();
  });
});

function insertTextInSection(doc: Y.Doc, sectionIndex: number, value: string): void {
  const section = doc.getXmlFragment("prosemirror").get(sectionIndex);
  if (!(section instanceof Y.XmlElement)) throw new Error("expected a section");
  const paragraph = section.get(1);
  if (!(paragraph instanceof Y.XmlElement)) throw new Error("expected a paragraph");
  const text = new Y.XmlText();
  text.insert(0, value);
  paragraph.insert(0, [text]);
}
