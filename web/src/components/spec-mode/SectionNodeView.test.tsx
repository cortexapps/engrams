import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, test, vi } from "vitest";
import { parseMarkdown, SPEC_FRAGMENT_NAME, type SpecTemplate } from "@engrams/spec-document";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import type { WebsocketProvider } from "y-websocket";
import * as Y from "yjs";

import { SectionList } from "./SectionList";
import { deriveSpecSurface } from "./spec-surface";
import type { SpecRail } from "@/hooks/useSpecRead";
import { ConnectedSpecCanvas } from "../spec/SpecCanvas";
import { createSpecProvider } from "../spec/SpecConnection";

const TEMPLATE: SpecTemplate = {
  sections: [{ id: "design", key: "design", title: "Proposed design" }],
};
const documents: Y.Doc[] = [];
const providers: WebsocketProvider[] = [];

afterEach(() => {
  for (const provider of providers.splice(0)) provider.destroy();
  for (const document of documents.splice(0)) document.destroy();
});

describe("SectionNodeView", () => {
  test("shares the derived state with the list and wires Keep, Revise, and Drop", async () => {
    const user = userEvent.setup();
    const doc = yDocument("## Proposed design\n\nThe parent bucket owns the billing counter.\n");
    const surface = deriveSpecSurface(rail("proposed"), doc, { readingSectionId: "none" });
    const onSetSectionState = vi.fn();
    const provider = specProvider(doc);
    const beforeDrop = doc.getXmlFragment(SPEC_FRAGMENT_NAME).toString();

    render(
      <>
        <SectionList surface={surface} onSelectSection={() => undefined} />
        <ConnectedSpecCanvas
          connection={{ doc, provider }}
          user={{ name: "Grace", color: "#2563eb" }}
          specId="spec-1"
          revision="17"
          surface={surface}
          showProvenance
          onSetSectionState={onSetSectionState}
        />
      </>,
    );

    await waitFor(() => expect(screen.getAllByRole("img", { name: "Proposed" })).toHaveLength(2));
    await user.click(screen.getByRole("button", { name: "Keep" }));
    await user.click(screen.getByRole("button", { name: "Revise" }));
    await user.click(screen.getByRole("button", { name: "Drop" }));

    expect(onSetSectionState.mock.calls.map(([action]) => action)).toEqual([
      { sectionId: "design", state: "settled" },
      { sectionId: "design", state: "proposed" },
      { sectionId: "design", state: "open" },
    ]);
    expect(screen.getByText("The parent bucket owns the billing counter.")).toBeTruthy();
    expect(doc.getXmlFragment(SPEC_FRAGMENT_NAME).toString()).toBe(beforeDrop);
  });

  test("shows the invitation only for an empty reached body and requires a reason", async () => {
    const user = userEvent.setup();
    const doc = yDocument("## Proposed design\n\n");
    const surface = deriveSpecSurface(rail("open"), doc);
    const onSetSectionState = vi.fn();
    const provider = specProvider(doc);

    render(
      <ConnectedSpecCanvas
        connection={{ doc, provider }}
        user={{ name: "Grace", color: "#2563eb" }}
        specId="spec-1"
        revision="17"
        surface={surface}
        showProvenance
        onSetSectionState={onSetSectionState}
      />,
    );

    expect(await screen.findByText(/Nothing here yet/)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Not this spec" }));
    const dialog = screen.getByRole("dialog");
    const confirm = within(dialog).getByRole("button", { name: "Confirm" });
    expect(confirm.hasAttribute("disabled")).toBe(true);
    await user.type(
      within(dialog).getByLabelText("Why does this section not belong in this spec?"),
      "The API does not change.",
    );
    await user.click(confirm);

    expect(onSetSectionState).toHaveBeenCalledWith({
      sectionId: "design",
      state: "n/a",
      reason: "The API does not change.",
    });
  });

  test("does not show the invitation when a body has words", async () => {
    const doc = yDocument("## Proposed design\n\nThe design already has a body.\n");
    const provider = specProvider(doc);
    render(
      <ConnectedSpecCanvas
        connection={{ doc, provider }}
        user={{ name: "Grace", color: "#2563eb" }}
        specId="spec-1"
        revision="17"
        surface={deriveSpecSurface(rail("open"), doc)}
        showProvenance
      />,
    );

    expect(await screen.findByText("The design already has a body.")).toBeTruthy();
    expect(screen.queryByText(/Nothing here yet/)).toBeNull();
  });

  test("uses surface provenance for both the settled chip and inline decoration", async () => {
    const doc = yDocument(
      "## Proposed design\n\nThe limiter lives in gateway/limits.rs @ 8f2c1a4.\n",
    );
    const provider = specProvider(doc);
    const surface = deriveSpecSurface(rail("settled"), doc);
    const view = render(
      <ConnectedSpecCanvas
        connection={{ doc, provider }}
        user={{ name: "Grace", color: "#2563eb" }}
        specId="spec-1"
        revision="17"
        surface={surface}
        showProvenance
      />,
    );

    expect(await screen.findByText("gateway/limits.rs @ 8f2c1a4")).toBeTruthy();
    await waitFor(() =>
      expect(view.container.querySelector(".spec-mode-provenance-mark")).toBeTruthy(),
    );

    view.rerender(
      <ConnectedSpecCanvas
        connection={{ doc, provider }}
        user={{ name: "Grace", color: "#2563eb" }}
        specId="spec-1"
        revision="17"
        surface={surface}
        showProvenance={false}
      />,
    );
    await waitFor(() =>
      expect(view.container.querySelector(".spec-mode-provenance-mark")).toBeNull(),
    );
  });
});

function yDocument(markdown: string): Y.Doc {
  const doc = new Y.Doc();
  documents.push(doc);
  prosemirrorToYXmlFragment(
    parseMarkdown(markdown, TEMPLATE),
    doc.getXmlFragment(SPEC_FRAGMENT_NAME),
  );
  return doc;
}

function specProvider(doc: Y.Doc): WebsocketProvider {
  const provider = createSpecProvider("spec-1", doc, {
    connect: false,
    location: { protocol: "https:", host: "engrams.test" },
    WebSocketPolyfill: window.WebSocket,
  });
  providers.push(provider);
  return provider;
}

function rail(state: "open" | "proposed" | "settled"): SpecRail {
  return {
    sections: [
      {
        id: "design",
        templateKey: "design",
        title: "Proposed design",
        state,
        naReason: null,
        allowNa: true,
        openQuestionCount: 0,
        settledBy: state === "settled" ? { id: "user-1", name: "Ada" } : null,
        stateChangedAt: "2026-08-13T12:00:00.000Z",
      },
    ],
    completeness: { complete: state === "settled" ? 1 : 0, total: 1 },
  };
}
