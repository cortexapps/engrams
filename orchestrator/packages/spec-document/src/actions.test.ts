import { describe, expect, test } from "bun:test";
import { Schema } from "prosemirror-model";

import { selectionSliceFingerprint } from "./actions.ts";

const testSchema = new Schema({
  nodes: {
    doc: { content: "paragraph+" },
    paragraph: {
      attrs: { role: { default: "body" } },
      content: "text*",
    },
    text: {},
  },
  marks: { emphasis: {} },
});

function document(options: { marked?: boolean; role?: string } = {}) {
  const marks = options.marked ? [testSchema.marks.emphasis!.create()] : undefined;
  return testSchema.nodes.doc!.create(null, [
    testSchema.nodes.paragraph!.create(
      { role: options.role ?? "body" },
      testSchema.text("same text", marks),
    ),
  ]);
}

describe("selectionSliceFingerprint", () => {
  test("is canonical for equal structured slices", () => {
    expect(selectionSliceFingerprint(document(), 1, 10)).toBe(
      selectionSliceFingerprint(document(), 1, 10),
    );
  });

  test("changes for marks and node attributes when text is unchanged", () => {
    const plain = selectionSliceFingerprint(document(), 1, 10);
    expect(selectionSliceFingerprint(document({ marked: true }), 1, 10)).not.toBe(plain);
    expect(selectionSliceFingerprint(document({ role: "caption" }), 1, 10)).not.toBe(plain);
  });
});
