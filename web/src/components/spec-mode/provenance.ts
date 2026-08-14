import { Extension, type Editor } from "@tiptap/core";
import { Plugin, PluginKey } from "@tiptap/pm/state";
import { Decoration, DecorationSet } from "@tiptap/pm/view";

import type { SpecSurfaceProvenanceRange } from "./spec-surface";

interface ProvenanceOptions {
  getRanges: () => readonly SpecSurfaceProvenanceRange[];
  isVisible: () => boolean;
}

export const provenancePluginKey = new PluginKey<DecorationSet>("spec-mode-provenance");

export const Provenance = Extension.create<ProvenanceOptions>({
  name: "specModeProvenance",
  addOptions: () => ({ getRanges: () => [], isVisible: () => true }),
  addProseMirrorPlugins() {
    const options = this.options;
    const decorations = (doc: Parameters<typeof DecorationSet.create>[0]) =>
      provenanceDecorations(doc, options.getRanges(), options.isVisible());
    return [
      new Plugin({
        key: provenancePluginKey,
        state: {
          init: (_, state) => decorations(state.doc),
          apply: (transaction, current) =>
            transaction.docChanged || transaction.getMeta(provenancePluginKey)
              ? decorations(transaction.doc)
              : current,
        },
        props: {
          decorations: (state) => provenancePluginKey.getState(state) ?? null,
        },
      }),
    ];
  },
});

export function refreshProvenance(editor: Editor): void {
  editor.view.dispatch(editor.state.tr.setMeta(provenancePluginKey, true));
}

export function provenanceDecorations(
  doc: Parameters<typeof DecorationSet.create>[0],
  ranges: readonly SpecSurfaceProvenanceRange[],
  visible: boolean,
): DecorationSet {
  if (!visible) return DecorationSet.empty;
  return DecorationSet.create(
    doc,
    ranges
      .filter(({ from, to }) => from >= 0 && to > from && to <= doc.content.size)
      .map(({ from, to, label }) =>
        Decoration.inline(from, to, {
          class: "spec-mode-provenance-mark",
          "data-provenance": label,
        }),
      ),
  );
}
