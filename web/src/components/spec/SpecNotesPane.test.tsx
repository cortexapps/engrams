import { render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, test, vi } from "vitest";
import {
  buildWorkingNotesDocument,
  SPEC_NOTES_FRAGMENT_NAME,
  type SpecWorkingNotes,
} from "@engrams/spec-document";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import type { WebsocketProvider } from "y-websocket";
import * as Y from "yjs";

import { SpecNotesPane } from "./SpecNotesPane";
import { createSpecProvider } from "./SpecCanvas";
import { SpecSectionTitleProvider } from "./section-titles";

const USER = { name: "Grace", color: "#2563eb" };
const TITLES = new Map([
  ["behavior", "Behavior"],
  ["api", "API surface"],
]);

const documents: Y.Doc[] = [];
const providers: WebsocketProvider[] = [];

afterEach(() => {
  for (const provider of providers.splice(0)) provider.destroy();
  for (const document of documents.splice(0)) document.destroy();
});

function notes(): SpecWorkingNotes {
  return {
    clusters: [
      {
        id: "burst",
        theme: "burst semantics",
        sectionIds: ["behavior", "api"],
        bullets: [
          {
            id: "b1",
            mark: "verified",
            kind: "observation",
            text: "orgs get a burst credit pool, refills daily",
            provenance: "your call, firm",
            agentText: "orgs get a burst credit pool, refills daily",
          },
          {
            id: "b2",
            mark: "contradicted",
            kind: "observation",
            text: "the limiter already handles bursts",
            provenance: "limits.rs @ 8f2c1a4: a per-user bucket",
            agentText: "the limiter already handles bursts",
          },
        ],
      },
      {
        id: "pile",
        theme: "untagged",
        sectionIds: [],
        bullets: [
          {
            id: "b3",
            mark: "unchecked",
            kind: "question",
            text: "how fine is the billing granularity?",
            provenance: null,
            agentText: "how fine is the billing granularity?",
          },
          {
            id: "b4",
            mark: "unchecked",
            kind: "tension",
            text: "should the meter feed the autoscaler?",
            provenance: null,
            agentText: "should the meter feed the autoscaler?",
          },
        ],
      },
    ],
  };
}

function paneUnder(input: { archived?: boolean; readOnly?: boolean; onDistill?: () => void } = {}) {
  const doc = new Y.Doc();
  documents.push(doc);
  prosemirrorToYXmlFragment(
    buildWorkingNotesDocument(notes()),
    doc.getXmlFragment(SPEC_NOTES_FRAGMENT_NAME),
  );
  const provider = createSpecProvider("spec-1", doc, {
    connect: false,
    location: { protocol: "https:", host: "engrams.test" },
    WebSocketPolyfill: window.WebSocket,
  });
  providers.push(provider);
  const pane = (readOnly: boolean) => (
    <SpecSectionTitleProvider titles={TITLES}>
      <SpecNotesPane
        doc={doc}
        provider={provider}
        user={USER}
        archived={input.archived ?? false}
        readOnly={readOnly}
        {...(input.onDistill ? { onDistill: input.onDistill } : {})}
      />
    </SpecSectionTitleProvider>
  );
  const view = render(pane(input.readOnly ?? false));
  return { ...view, setReadOnly: (readOnly: boolean) => view.rerender(pane(readOnly)) };
}

describe("the working notes pane", () => {
  test("renders the verification marks, the receipts and the destination tags", async () => {
    paneUnder();

    const pane = await screen.findByLabelText("Working notes text");
    await waitFor(() => expect(pane.querySelectorAll(".spec-notes-bullet")).toHaveLength(4));
    expect(pane.querySelector('[data-bullet-id="b1"] .spec-notes-mark')?.textContent).toContain(
      "✓",
    );
    expect(pane.querySelector('[data-bullet-id="b2"] .spec-notes-mark')?.textContent).toContain(
      "✗",
    );
    expect(pane.querySelector('[data-bullet-id="b3"] .spec-notes-mark')?.textContent).toContain(
      "?",
    );
    // A contradicted bullet keeps the claim and the receipt.
    const refuted = pane.querySelector('[data-bullet-id="b2"]');
    expect(refuted?.textContent).toContain("the limiter already handles bursts");
    expect(refuted?.textContent).toContain("limits.rs @ 8f2c1a4: a per-user bucket");
    expect(screen.getByText("→ §Behavior")).toBeTruthy();
    expect(screen.getByText("→ §API surface")).toBeTruthy();
    // The kind labels carry the notes' own vocabulary onto the canvas.
    expect(screen.getByText("question for you")).toBeTruthy();
    expect(screen.getByText("tension")).toBeTruthy();
    expect(screen.queryByText("requirement candidate")).toBeNull();
  });

  test("the header carries R22's untagged pile", async () => {
    paneUnder();

    expect(await screen.findByText(/untagged pile: 2/)).toBeTruthy();
  });

  test("a bullet is editable, and the notes read as scratch", async () => {
    paneUnder();

    const editor = await screen.findByLabelText("Working notes text");
    expect(editor.getAttribute("contenteditable")).toBe("true");
    expect(editor.className).toContain("spec-notes-editor");
    // The agent's model is chrome, not text a keystroke reaches.
    expect(editor.querySelector(".spec-notes-cluster-head")?.getAttribute("contenteditable")).toBe(
      "false",
    );
    expect(editor.querySelector(".spec-notes-receipt")?.getAttribute("contenteditable")).toBe(
      "false",
    );
  });

  test("the author closes the stage from the pane", async () => {
    const onDistill = vi.fn();
    paneUnder({ onDistill });

    const button = await screen.findByRole("button", { name: "Draft the spec" });
    button.click();

    expect(onDistill).toHaveBeenCalledTimes(1);
  });

  test("read-only reaches the mounted editor, in both directions", async () => {
    // The width test settles one render after mount, so the pane can be created
    // writable and told to stop. The creation option alone would miss that.
    const view = paneUnder();
    const editor = await screen.findByLabelText("Working notes text");
    await waitFor(() => expect(editor.getAttribute("contenteditable")).toBe("true"));

    view.setReadOnly(true);
    await waitFor(() => expect(editor.getAttribute("contenteditable")).toBe("false"));

    view.setReadOnly(false);
    await waitFor(() => expect(editor.getAttribute("contenteditable")).toBe("true"));
  });

  test("the archive is read-only, and it says why", async () => {
    const onDistill = vi.fn();
    paneUnder({ archived: true, onDistill });

    const editor = await screen.findByLabelText("Notes archive text");
    expect(editor.getAttribute("contenteditable")).toBe("false");
    expect(screen.getByText("Notes archive — not the spec")).toBeTruthy();
    expect(screen.getByText(/read-only · never published/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Draft the spec" })).toBeNull();
    expect(screen.queryByText(/untagged pile/)).toBeNull();
  });
});
