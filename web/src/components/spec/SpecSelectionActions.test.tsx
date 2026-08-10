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
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import * as Y from "yjs";

import { SpecSelectionMenu, createSpecSelectionSpan } from "./SpecSelectionActions";

function selectionFixture(): SpecSelectionSpan {
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
  try {
    prosemirrorToYXmlFragment(withText, ydoc.getXmlFragment(SPEC_FRAGMENT_NAME));
    let from = -1;
    withText.descendants((node, position) => {
      if (node.isText && node.text === "Retry forever.") from = position;
    });
    const span = createSpecSelectionSpan(withText, ydoc, from, from + "Retry".length);
    if (!span) throw new Error("The test selection is missing.");
    return span;
  } finally {
    ydoc.destroy();
  }
}

describe("SpecSelectionMenu", () => {
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
      selectedText: "Retry",
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
