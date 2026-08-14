import { render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { parseMarkdown, SPEC_FRAGMENT_NAME, type SpecTemplate } from "@engrams/spec-document";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import type { WebsocketProvider } from "y-websocket";
import * as Y from "yjs";

import { deriveSpecSurface } from "@/components/spec-mode/spec-surface";
import type { SpecRail } from "@/hooks/useSpecRead";
import { ConnectedSpecCanvas } from "./SpecCanvas";
import { createSpecProvider } from "./SpecConnection";

const USER = { id: "grace", name: "Grace", color: "#2563eb" };
const TEMPLATE: SpecTemplate = {
  sections: [{ id: "context", key: "context", title: "Context" }],
};
const MARKDOWN = "## Context\n\nThe limiter retries {{open-question:q1}} forever.\n";
const documents: Y.Doc[] = [];
const providers: WebsocketProvider[] = [];
const desktopWidth = window.innerWidth;

function setWidth(pixels: number) {
  Object.defineProperty(window, "innerWidth", {
    configurable: true,
    writable: true,
    value: pixels,
  });
}

beforeEach(() => setWidth(desktopWidth));

afterEach(() => {
  setWidth(desktopWidth);
  for (const provider of providers.splice(0)) provider.destroy();
  for (const document of documents.splice(0)) document.destroy();
});

function element(parent: Y.XmlFragment, index: number): Y.XmlElement {
  const child = parent.get(index);
  if (!(child instanceof Y.XmlElement)) throw new Error(`child ${index} is not an element`);
  return child;
}

/**
 * Mount the canvas over a live provider, with one section that carries an
 * inline open-question marker.
 */
function canvasUnder() {
  const doc = new Y.Doc();
  documents.push(doc);
  prosemirrorToYXmlFragment(
    parseMarkdown(MARKDOWN, TEMPLATE),
    doc.getXmlFragment(SPEC_FRAGMENT_NAME),
  );
  const provider = createSpecProvider("spec-1", doc, {
    connect: false,
    location: { protocol: "https:", host: "engrams.test" },
    WebSocketPolyfill: window.WebSocket,
  });
  providers.push(provider);
  const rail: SpecRail = {
    sections: [
      {
        id: "context",
        templateKey: "context",
        title: "Context",
        state: "open",
        naReason: null,
        allowNa: true,
        openQuestionCount: 1,
        settledBy: null,
        stateChangedAt: null,
      },
    ],
    completeness: { complete: 0, total: 1 },
  };
  const view = render(
    <ConnectedSpecCanvas
      connection={{ doc, provider }}
      user={USER}
      specId="spec-1"
      revision="17"
      surface={deriveSpecSurface(rail, doc)}
      showProvenance
    />,
  );
  return { ...view, doc };
}

/** Write into the shared document the way another author's update does. */
function remoteEdit(doc: Y.Doc, text: string) {
  const paragraph = element(element(doc.getXmlFragment(SPEC_FRAGMENT_NAME), 0), 1);
  paragraph.insert(paragraph.length, [new Y.XmlText(text)]);
}

describe("the spec canvas at width", () => {
  test("at 420px the mounted editor is not editable", async () => {
    setWidth(420);
    canvasUnder();

    const editor = await screen.findByLabelText("Collaborative spec document");
    await waitFor(() => expect(editor.getAttribute("contenteditable")).toBe("false"));
    expect(editor.className).toContain("spec-canvas-editor");
    expect(screen.getByText("Read-only on a small screen. The text stays live.")).toBeTruthy();
    expect(screen.queryByRole("toolbar", { name: "Spec formatting" })).toBeNull();
  });

  test("read-only is not disconnected: another author's text still lands", async () => {
    setWidth(420);
    const { doc } = canvasUnder();

    const editor = await screen.findByLabelText("Collaborative spec document");
    await waitFor(() => expect(editor.getAttribute("contenteditable")).toBe("false"));

    remoteEdit(doc, " Ada added a sentence.");

    await waitFor(() => expect(editor.textContent).toContain("Ada added a sentence."));
    expect(editor.getAttribute("contenteditable")).toBe("false");
  });

  test("the desktop keeps the WYSIWYG", async () => {
    canvasUnder();

    const editor = await screen.findByLabelText("Collaborative spec document");
    await waitFor(() => expect(editor.getAttribute("contenteditable")).toBe("true"));
    expect(screen.getByRole("toolbar", { name: "Spec formatting" })).toBeTruthy();
  });

  test.each([
    ["420px", 420],
    ["a desktop width", desktopWidth],
  ])("the open-question marker stays inline at %s", async (_label, width) => {
    setWidth(width);
    canvasUnder();

    const editor = await screen.findByLabelText("Collaborative spec document");
    await waitFor(() =>
      expect(editor.querySelector('.spec-mode-open-question[data-question-id="q1"]')).toBeTruthy(),
    );
    expect(editor.textContent).toContain("Open question");
  });
});
