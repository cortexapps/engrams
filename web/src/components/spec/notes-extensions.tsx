import { Extension, Node, type Attributes, type NodeConfig } from "@tiptap/core";
import { Plugin, type EditorState, type Transaction } from "@tiptap/pm/state";
import { NodeViewContent, NodeViewWrapper, ReactNodeViewRenderer } from "@tiptap/react";
import type { NodeViewProps } from "@tiptap/react";
import type { Node as ProseMirrorNode, NodeSpec } from "@tiptap/pm/model";
import {
  isSpecNoteKind,
  isSpecNoteMark,
  SPEC_NOTE_MARK_GLYPHS,
  specNotesNodeSpecs,
  type SpecNoteKind,
  type SpecNoteMark,
} from "@engrams/spec-document";
import { useSpecSectionTitles } from "./section-titles";

/**
 * The working-notes editor (ADR 0114 D6, R21-R23).
 *
 * A person edits the words in a bullet, and nothing else: the marks, the
 * receipts, the themes and the destination tags are the agent's model, so they
 * render as chrome that no keystroke reaches. `NotesStructure` is the guard —
 * a local transaction that changes the structure is refused before it can
 * reach Yjs, where the server would refuse it anyway and leave this browser
 * holding an update that never lands.
 */

function nodeSpec(name: keyof typeof specNotesNodeSpecs): NodeSpec {
  const spec = specNotesNodeSpecs[name];
  if (!spec) throw new Error(`The shared notes schema has no ${String(name)} node`);
  return spec;
}

function sharedConfig(name: keyof typeof specNotesNodeSpecs): Partial<NodeConfig> {
  const spec = nodeSpec(name);
  return {
    content: spec.content,
    marks: spec.marks,
    group: spec.group,
    inline: spec.inline,
    atom: spec.atom,
    isolating: spec.isolating,
    defining: spec.defining,
    addAttributes: () => sharedAttributes(spec),
  };
}

function sharedAttributes(spec: NodeSpec): Attributes {
  return Object.fromEntries(
    Object.entries(spec.attrs ?? {}).map(([name, attribute]) => [
      name,
      Object.hasOwn(attribute, "default")
        ? { default: attribute.default }
        : { default: undefined, isRequired: true },
    ]),
  );
}

const NotesDocument = Node.create({
  name: "doc",
  topNode: true,
  ...sharedConfig("doc"),
});

/** The theme line and the destination tags: the agent's clustering. */
function NoteCluster({ node }: NodeViewProps) {
  const titles = useSpecSectionTitles();
  const sectionIds = Array.isArray(node.attrs.sectionIds)
    ? node.attrs.sectionIds.filter((value: unknown): value is string => typeof value === "string")
    : [];
  return (
    <NodeViewWrapper className="spec-notes-cluster" data-cluster-id={String(node.attrs.id)}>
      <div className="spec-notes-cluster-head" contentEditable={false}>
        <span className="spec-notes-theme">{String(node.attrs.theme)}</span>
        {sectionIds.map((sectionId) => (
          <span className="spec-notes-tag" key={sectionId}>
            → §{titles.get(sectionId) ?? "unknown section"}
          </span>
        ))}
        {sectionIds.length === 0 && (
          <span className="spec-notes-tag spec-notes-tag-empty">no destination yet</span>
        )}
      </div>
      <NodeViewContent<"div"> as="div" className="spec-notes-bullets" role="list" />
    </NodeViewWrapper>
  );
}

const KIND_LABELS: Readonly<Record<SpecNoteKind, string | null>> = {
  observation: null,
  requirement: "requirement candidate",
  tension: "tension",
  question: "question for you",
};

const MARK_NAMES: Readonly<Record<SpecNoteMark, string>> = {
  verified: "verified",
  contradicted: "contradicted",
  unchecked: "unchecked",
};

/** One bullet: the mark and the receipt are chrome; the words are editable. */
function NoteBullet({ node }: NodeViewProps) {
  const mark = isSpecNoteMark(node.attrs.mark) ? node.attrs.mark : "unchecked";
  const kind = isSpecNoteKind(node.attrs.kind) ? node.attrs.kind : "observation";
  const provenance = typeof node.attrs.provenance === "string" ? node.attrs.provenance : null;
  const kindLabel = KIND_LABELS[kind];
  const corrected =
    typeof node.attrs.agentText === "string" && node.attrs.agentText !== node.textContent;
  return (
    <NodeViewWrapper
      role="listitem"
      className="spec-notes-bullet"
      data-mark={mark}
      data-bullet-id={String(node.attrs.id)}
      {...(corrected ? { "data-corrected": "true" } : {})}
    >
      <span className={`spec-notes-mark spec-notes-mark-${mark}`} contentEditable={false}>
        <span aria-hidden="true">{SPEC_NOTE_MARK_GLYPHS[mark]}</span>
        <span className="sr-only">{MARK_NAMES[mark]}</span>
      </span>
      <NodeViewContent<"span"> as="span" className="spec-notes-text" />
      {provenance !== null && (
        <span className="spec-notes-receipt" contentEditable={false}>
          {provenance}
        </span>
      )}
      {kindLabel !== null && (
        <span className="spec-notes-kind" contentEditable={false}>
          {kindLabel}
        </span>
      )}
    </NodeViewWrapper>
  );
}

const NoteClusterNode = Node.create({
  name: "noteCluster",
  ...sharedConfig("noteCluster"),
  parseHTML: () => [{ tag: "section[data-cluster-id]" }],
  renderHTML: ({ node }) => ["section", { "data-cluster-id": node.attrs.id }, 0],
  addNodeView: () => ReactNodeViewRenderer(NoteCluster),
});

const NoteBulletNode = Node.create({
  name: "noteBullet",
  ...sharedConfig("noteBullet"),
  parseHTML: () => [{ tag: "div[data-bullet-id]" }],
  renderHTML: ({ node }) => ["div", { "data-bullet-id": node.attrs.id }, 0],
  addNodeView: () => ReactNodeViewRenderer(NoteBullet),
});

const NotesText = Node.create({
  name: "text",
  ...sharedConfig("text"),
});

/** The bullet set and every attribute, as the notes now hold them. */
export function notesStructure(doc: ProseMirrorNode): string | null {
  const clusters: unknown[] = [];
  let valid = true;
  doc.forEach((cluster) => {
    if (cluster.type.name !== "noteCluster" || typeof cluster.attrs.id !== "string") {
      valid = false;
      return;
    }
    const bullets: unknown[] = [];
    cluster.forEach((bullet) => {
      if (bullet.type.name !== "noteBullet" || typeof bullet.attrs.id !== "string") {
        valid = false;
        return;
      }
      bullets.push({
        id: bullet.attrs.id,
        mark: bullet.attrs.mark,
        kind: bullet.attrs.kind,
        provenance: bullet.attrs.provenance,
        agentText: bullet.attrs.agentText,
      });
    });
    clusters.push({
      id: cluster.attrs.id,
      theme: cluster.attrs.theme,
      sectionIds: cluster.attrs.sectionIds,
      bullets,
    });
  });
  return valid ? JSON.stringify(clusters) : null;
}

/**
 * True when the Yjs binding produced this transaction.
 *
 * The binding's plugin key is read from the editor's own plugin list rather
 * than imported: `new PluginKey("y-sync")` numbers its key per process, so a
 * second copy of y-prosemirror in the tree would answer to a different string
 * and every remote update would look local.
 */
export function isBindingTransaction(transaction: Transaction, state: EditorState): boolean {
  return state.plugins.some((plugin) => {
    // `key` is the real field a keyed ProseMirror plugin carries; the published
    // types leave it out, so the shape is widened for the field that exists.
    const key = (plugin as Plugin & { key?: string }).key;
    return (
      typeof key === "string" && key.startsWith("y-sync$") && transaction.getMeta(key) !== undefined
    );
  });
}

export function createNotesStructurePlugin(): Plugin {
  return new Plugin({
    filterTransaction: (transaction, state) => {
      if (!transaction.docChanged) return true;
      // An update the server already accepted arrives through the Yjs binding.
      // Only a local keystroke is constrained.
      if (isBindingTransaction(transaction, state)) return true;
      const before = notesStructure(transaction.before);
      const after = notesStructure(transaction.doc);
      return before !== null && after !== null && before === after;
    },
  });
}

export const NotesStructure = Extension.create({
  name: "notesStructure",
  addProseMirrorPlugins() {
    return [createNotesStructurePlugin()];
  },
});

export const specNotesExtensions = [
  NotesDocument,
  NoteClusterNode,
  NoteBulletNode,
  NotesText,
  NotesStructure,
];
