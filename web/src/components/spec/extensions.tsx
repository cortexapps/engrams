import { Node, Extension, type Attributes, type NodeConfig } from "@tiptap/core";
import { Plugin } from "@tiptap/pm/state";
import { NodeViewWrapper, ReactNodeViewRenderer, type NodeViewProps } from "@tiptap/react";
import { specNodeSpecs } from "@engrams/spec-document";
import type { Node as ProseMirrorNode, NodeSpec } from "@tiptap/pm/model";

function nodeSpec(name: keyof typeof specNodeSpecs): NodeSpec {
  const spec = specNodeSpecs[name];
  if (!spec) throw new Error(`The shared spec schema has no ${name} node`);
  return spec;
}

function sharedConfig(name: keyof typeof specNodeSpecs): Partial<NodeConfig> {
  const spec = nodeSpec(name);
  return {
    content: spec.content,
    marks: spec.marks,
    group: spec.group,
    inline: spec.inline,
    atom: spec.atom,
    selectable: spec.selectable,
    draggable: spec.draggable,
    code: spec.code,
    defining: spec.defining,
    isolating: spec.isolating,
    whitespace: spec.whitespace,
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

const SpecDocument = Node.create({
  name: "doc",
  topNode: true,
  ...sharedConfig("doc"),
});

const SpecSection = Node.create({
  name: "section",
  ...sharedConfig("section"),
  parseHTML: () => [{ tag: "section[data-spec-section]" }],
  renderHTML: ({ node }) => [
    "section",
    {
      "data-spec-section": node.attrs.id,
      "data-template-section-key": node.attrs.templateSectionKey,
    },
    0,
  ],
});

const SpecSectionHeading = Node.create({
  name: "sectionHeading",
  ...sharedConfig("sectionHeading"),
  parseHTML: () => [{ tag: "h2[data-spec-section-heading]" }],
  renderHTML: () => ["h2", { "data-spec-section-heading": "" }, 0],
});

const SpecParagraph = Node.create({
  name: "paragraph",
  ...sharedConfig("paragraph"),
  parseHTML: () => [{ tag: "p" }],
  renderHTML: () => ["p", 0],
});

const SpecHeading = Node.create({
  name: "heading",
  ...sharedConfig("heading"),
  parseHTML: () => [3, 4, 5, 6].map((level) => ({ tag: `h${level}`, attrs: { level } })),
  renderHTML: ({ node }) => [`h${clampHeadingLevel(node.attrs.level)}`, 0],
});

const SpecCodeBlock = Node.create({
  name: "codeBlock",
  ...sharedConfig("codeBlock"),
  parseHTML: () => [
    {
      tag: "pre",
      getAttrs: (element) => ({
        language: element instanceof HTMLElement ? (element.dataset.language ?? "") : "",
      }),
    },
  ],
  renderHTML: ({ node }) => ["pre", { "data-language": node.attrs.language }, ["code", 0]],
});

const SpecText = Node.create({
  name: "text",
  ...sharedConfig("text"),
});

function OpenQuestionStub({ node }: NodeViewProps) {
  return (
    <NodeViewWrapper
      as="span"
      className="spec-open-question"
      data-question-id={String(node.attrs.questionId)}
      contentEditable={false}
    >
      <span aria-hidden="true">?</span>
      <span>Open question</span>
    </NodeViewWrapper>
  );
}

const SpecOpenQuestion = Node.create({
  name: "openQuestion",
  ...sharedConfig("openQuestion"),
  parseHTML: () => [{ tag: "span[data-spec-open-question]" }],
  renderHTML: ({ node }) => [
    "span",
    { "data-spec-open-question": node.attrs.questionId },
    "Open question",
  ],
  addNodeView: () => ReactNodeViewRenderer(OpenQuestionStub, { as: "span" }),
});

function DiagramBlockStub({ node }: NodeViewProps) {
  return (
    <NodeViewWrapper className="spec-diagram-block" contentEditable={false}>
      <div className="spec-diagram-label">{String(node.attrs.kind || "diagram")}</div>
      <pre>{String(node.attrs.source || "Diagram preview will appear here.")}</pre>
    </NodeViewWrapper>
  );
}

const SpecDiagramBlock = Node.create({
  name: "diagramBlock",
  ...sharedConfig("diagramBlock"),
  parseHTML: () => [{ tag: "figure[data-spec-diagram]" }],
  renderHTML: ({ node }) => [
    "figure",
    {
      "data-spec-diagram": node.attrs.blockId,
      "data-kind": node.attrs.kind,
      "data-source": node.attrs.source,
    },
  ],
  addNodeView: () => ReactNodeViewRenderer(DiagramBlockStub),
});

export function sectionIds(doc: ProseMirrorNode): string[] | null {
  const ids: string[] = [];
  let valid = true;
  doc.forEach((node) => {
    if (node.type.name !== "section" || typeof node.attrs.id !== "string") {
      valid = false;
      return;
    }
    ids.push(node.attrs.id);
  });
  return valid ? ids : null;
}

export function hasSameSectionStructure(before: ProseMirrorNode, after: ProseMirrorNode): boolean {
  const beforeIds = sectionIds(before);
  const afterIds = sectionIds(after);
  return (
    beforeIds !== null &&
    afterIds !== null &&
    beforeIds.length === afterIds.length &&
    beforeIds.every((id, index) => id === afterIds[index])
  );
}

export function createSectionStructurePlugin(): Plugin {
  return new Plugin({
    filterTransaction: (transaction) =>
      !transaction.docChanged || hasSameSectionStructure(transaction.before, transaction.doc),
  });
}

export const SectionStructure = Extension.create({
  name: "sectionStructure",
  addProseMirrorPlugins() {
    return [createSectionStructurePlugin()];
  },
});

export const specNodeExtensions = [
  SpecDocument,
  SpecSection,
  SpecSectionHeading,
  SpecParagraph,
  SpecHeading,
  SpecCodeBlock,
  SpecDiagramBlock,
  SpecOpenQuestion,
  SpecText,
  SectionStructure,
];

function clampHeadingLevel(value: unknown): number {
  const level = typeof value === "number" ? value : 3;
  return Math.min(6, Math.max(3, level));
}
