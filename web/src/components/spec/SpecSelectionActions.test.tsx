import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, test, vi } from "vitest";
import {
  SPEC_FRAGMENT_NAME,
  createTemplateDocument,
  findSection,
  schema,
  type SpecSelectionSpan,
} from "@engrams/spec-document";
import { Transform } from "@tiptap/pm/transform";
import { getSchema } from "@tiptap/core";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import * as Y from "yjs";

import { SpecSelectionMenu, createSpecSelectionSpan, sameSelection } from "./SpecSelectionActions";
import { specNodeExtensions } from "./extensions";

function selectionDocumentFixture() {
  const document = createTemplateDocument({
    sections: [{ id: "failure-modes", key: "failure-modes", title: "Failure modes" }],
  });
  const section = findSection(document, "failure-modes");
  if (!section) throw new Error("The test section is missing.");
  const withText = new Transform(document).insert(
    section.position + section.node.nodeSize - 2,
    schema.text("Retry forever."),
  ).doc;
  const ydoc = new Y.Doc();
  prosemirrorToYXmlFragment(withText, ydoc.getXmlFragment(SPEC_FRAGMENT_NAME));
  let headingStart = -1;
  let bodyStart = -1;
  withText.descendants((node, position) => {
    if (node.isText && node.text === "Failure modes") headingStart = position;
    if (node.isText && node.text === "Retry forever.") bodyStart = position;
  });
  if (headingStart < 0 || bodyStart < 0) throw new Error("The test text is missing.");
  return { document: withText, ydoc, headingStart, bodyStart };
}

function selectionFixture(): SpecSelectionSpan {
  const { document, ydoc, bodyStart } = selectionDocumentFixture();
  try {
    const span = createSpecSelectionSpan(
      document,
      ydoc,
      bodyStart,
      bodyStart + "Retry".length,
      "00000000-0000-4000-8000-000000000112",
      "7",
    );
    if (!span) throw new Error("The test selection is missing.");
    return span;
  } finally {
    ydoc.destroy();
  }
}

describe("SpecSelectionMenu", () => {
  test("rejects a selection wholly in the section heading", () => {
    const { document, ydoc, headingStart } = selectionDocumentFixture();
    try {
      expect(
        createSpecSelectionSpan(
          document,
          ydoc,
          headingStart,
          headingStart + "Failure".length,
          "00000000-0000-4000-8000-000000000112",
          "7",
        ),
      ).toBeNull();
    } finally {
      ydoc.destroy();
    }
  });

  test("rejects a selection that crosses from the heading into the body", () => {
    const { document, ydoc, headingStart, bodyStart } = selectionDocumentFixture();
    try {
      expect(
        createSpecSelectionSpan(
          document,
          ydoc,
          headingStart,
          bodyStart + "Retry".length,
          "00000000-0000-4000-8000-000000000112",
          "7",
        ),
      ).toBeNull();
    } finally {
      ydoc.destroy();
    }
  });

  test("allows a selection wholly in the section body", () => {
    expect(selectionFixture()).toMatchObject({
      sectionId: "failure-modes",
      selectedText: "Retry",
    });
  });

  test("allows a body selection from the TipTap editor schema", () => {
    const { document, ydoc, bodyStart } = selectionDocumentFixture();
    try {
      const editorSchema = getSchema(specNodeExtensions);
      const editorDocument = editorSchema.nodeFromJSON(document.toJSON());
      expect(editorDocument.type).not.toBe(document.type);
      expect(
        createSpecSelectionSpan(
          editorDocument,
          ydoc,
          bodyStart,
          bodyStart + "Retry".length,
          "00000000-0000-4000-8000-000000000112",
          "7",
        ),
      ).toMatchObject({
        sectionId: "failure-modes",
        selectedText: "Retry",
      });
    } finally {
      ydoc.destroy();
    }
  });

  test("keeps an equal transaction snapshot state-stable", () => {
    const selection = selectionFixture();

    expect(sameSelection(selection, { ...selection })).toBe(true);
    expect(
      sameSelection(selection, {
        ...selection,
        sliceFingerprint: "0".repeat(64),
      }),
    ).toBe(false);
  });

  test("appears for a selection and sends the full span payload", async () => {
    const user = userEvent.setup();
    const onAction = vi.fn();
    const selection = selectionFixture();
    const { rerender } = render(
      <SpecSelectionMenu
        specId="00000000-0000-4000-8000-000000000112"
        selection={null}
        onAction={onAction}
      />,
    );

    expect(screen.queryByRole("dialog", { name: "Actions for selected text" })).toBeNull();
    rerender(
      <SpecSelectionMenu
        specId="00000000-0000-4000-8000-000000000112"
        selection={selection}
        onAction={onAction}
      />,
    );

    expect(screen.getByRole("dialog", { name: "Actions for selected text" })).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Refine" }));
    expect(onAction).toHaveBeenCalledWith({
      specId: "00000000-0000-4000-8000-000000000112",
      action: "refine",
      instruction: "Refine this passage.",
      span: selection,
    });
    expect(selection).toMatchObject({
      sectionId: "failure-modes",
      specId: "00000000-0000-4000-8000-000000000112",
      revision: "7",
      selectedText: "Retry",
      sliceFingerprint: expect.stringMatching(/^[0-9a-f]{64}$/),
    });
    expect(selection.startAnchor).toMatch(/^yjs-section:\/\/failure-modes\//);
    expect(selection.endAnchor).toMatch(/^yjs-section:\/\/failure-modes\//);
  });

  test("sends a free-form instruction without a role-specific API", async () => {
    const user = userEvent.setup();
    const onAction = vi.fn();
    const selection = selectionFixture();
    render(
      <SpecSelectionMenu
        specId="00000000-0000-4000-8000-000000000112"
        selection={selection}
        onAction={onAction}
      />,
    );

    await user.type(
      screen.getByRole("textbox", { name: "Custom selection instruction" }),
      "Use a bounded retry.",
    );
    await user.click(screen.getByRole("button", { name: "Send" }));
    expect(onAction).toHaveBeenCalledWith({
      specId: "00000000-0000-4000-8000-000000000112",
      action: "custom",
      instruction: "Use a bounded retry.",
      span: selection,
    });
  });
});
