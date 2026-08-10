import { describe, expect, test } from "bun:test";

import {
  findQuestionMarker,
  insertQuestionMarker,
  QuestionResolutionConflictError,
  resolveQuestionMarker,
} from "./questions.ts";
import { createTemplateDocument, renderMarkdown } from "./schema.ts";

const SECTION_ID = "failure-modes";
const QUESTION_ID = "00000000-0000-4000-8000-000000000011";

function documentWithQuestion(text = "Before after") {
  const base = createTemplateDocument({
    sections: [{ id: SECTION_ID, key: SECTION_ID, title: "Failure modes" }],
  });
  const paragraph = base.firstChild!.child(1);
  if (text.length === 0) {
    const marker = base.type.schema.nodes.openQuestion!.create({ questionId: QUESTION_ID });
    return base.type.create(null, [
      base.firstChild!.type.create(base.firstChild!.attrs, [
        base.firstChild!.firstChild!,
        paragraph.type.create(null, marker),
      ]),
    ]);
  }
  const withText = base.type.create(null, [
    base.firstChild!.type.create(base.firstChild!.attrs, [
      base.firstChild!.firstChild!,
      paragraph.type.create(null, text.length > 0 ? base.type.schema.text(text) : undefined),
    ]),
  ]);
  let textPosition = 0;
  withText.descendants((node, position) => {
    if (node.isText) textPosition = position;
  });
  return insertQuestionMarker(
    withText,
    SECTION_ID,
    textPosition + "Before ".length,
    QUESTION_ID,
    "question-request",
  );
}

describe("question document markers", () => {
  test("adds word boundaries around an inline answer", () => {
    const first = resolveQuestionMarker(
      documentWithQuestion(),
      SECTION_ID,
      QUESTION_ID,
      "Use three tries.",
    );

    expect(renderMarkdown(first.doc)).toBe("## Failure modes\n\nBefore Use three tries. after\n");
  });

  test("absorbs Markdown blocks and keeps a stable replay marker", () => {
    const answerMarkdown = "Use three tries.\n\n```text\nstop\n```";
    const first = resolveQuestionMarker(
      documentWithQuestion(""),
      SECTION_ID,
      QUESTION_ID,
      answerMarkdown,
    );
    const rendered = renderMarkdown(first.doc);

    expect(first.replayed).toBe(false);
    expect(rendered).toContain("Use three tries.");
    expect(rendered).toContain("```text\nstop\n```");
    expect(rendered).not.toContain("{{open-question:");
    expect(findQuestionMarker(first.doc, QUESTION_ID, SECTION_ID)?.node.attrs).toMatchObject({
      resolved: true,
      answerMarkdown,
    });

    const replay = resolveQuestionMarker(first.doc, SECTION_ID, QUESTION_ID, answerMarkdown);
    expect(replay.replayed).toBe(true);
    expect(replay.doc).toBe(first.doc);
  });

  test("rejects a different answer after absorption", () => {
    const first = resolveQuestionMarker(
      documentWithQuestion(),
      SECTION_ID,
      QUESTION_ID,
      "Use three tries.",
    );

    expect(() =>
      resolveQuestionMarker(first.doc, SECTION_ID, QUESTION_ID, "Retry forever."),
    ).toThrow(QuestionResolutionConflictError);
  });
});
