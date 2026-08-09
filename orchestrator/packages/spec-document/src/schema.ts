import { Fragment, Node as ProseMirrorNode, Schema, type NodeSpec } from "prosemirror-model";
import { Transform } from "prosemirror-transform";

export const SPEC_FRAGMENT_NAME = "prosemirror";

export interface SpecTemplateSection {
  id: string;
  key: string;
  title: string;
}

export interface SpecTemplate {
  sections: readonly SpecTemplateSection[];
}

export const specNodeSpecs: Readonly<Record<string, NodeSpec>> = {
  doc: { content: "section+" },
  section: {
    content: "sectionHeading block+",
    attrs: {
      id: { default: null },
      templateSectionKey: { default: null },
    },
    isolating: true,
  },
  sectionHeading: {
    content: "inline*",
    marks: "",
    defining: true,
  },
  paragraph: {
    content: "inline*",
    group: "block",
  },
  heading: {
    attrs: { level: { default: 3 } },
    content: "inline*",
    group: "block",
    defining: true,
  },
  codeBlock: {
    attrs: { language: { default: "" } },
    content: "text*",
    marks: "",
    group: "block",
    code: true,
    defining: true,
  },
  diagramBlock: {
    attrs: {
      blockId: {},
      kind: {},
      source: { default: "" },
    },
    group: "block",
    atom: true,
    isolating: true,
  },
  openQuestion: {
    attrs: { questionId: {} },
    group: "inline",
    inline: true,
    atom: true,
  },
  text: { group: "inline" },
};

export const schema = new Schema({ nodes: specNodeSpecs });

function textNode(value: string): ProseMirrorNode | null {
  return value.length > 0 ? schema.text(value) : null;
}

function paragraph(value = ""): ProseMirrorNode {
  const text = textNode(value);
  return schema.nodes.paragraph!.create(null, text ? [text] : undefined);
}

export function createTemplateDocument(template: SpecTemplate): ProseMirrorNode {
  if (template.sections.length === 0) {
    throw new Error("A spec template must contain at least one section");
  }

  return schema.nodes.doc!.create(
    null,
    template.sections.map((section) =>
      schema.nodes.section!.create(
        { id: section.id, templateSectionKey: section.key },
        [schema.nodes.sectionHeading!.create(null, schema.text(section.title)), paragraph()],
      ),
    ),
  );
}

export interface LocatedSection {
  node: ProseMirrorNode;
  position: number;
}

export function findSection(doc: ProseMirrorNode, sectionId: string): LocatedSection | null {
  let found: LocatedSection | null = null;
  doc.forEach((node, offset) => {
    if (found === null && node.type === schema.nodes.section && node.attrs.id === sectionId) {
      found = { node, position: offset };
    }
  });
  return found;
}

export function replaceSection(
  doc: ProseMirrorNode,
  sectionId: string,
  replacement: ProseMirrorNode,
): ProseMirrorNode {
  const current = findSection(doc, sectionId);
  if (!current) throw new Error(`Unknown spec section: ${sectionId}`);
  if (replacement.type !== schema.nodes.section) {
    throw new Error("A section replacement must be a section node");
  }
  if (replacement.attrs.id !== sectionId) {
    throw new Error("A section replacement cannot change the section id");
  }

  return new Transform(doc)
    .replaceWith(current.position, current.position + current.node.nodeSize, replacement)
    .doc;
}

function escapeText(value: string): string {
  return value.replace(/([\\`*_{}\[\]<>])/g, "\\$1");
}

function renderInline(node: ProseMirrorNode): string {
  let result = "";
  node.forEach((child) => {
    if (child.isText) {
      result += escapeText(child.text ?? "");
    } else if (child.type === schema.nodes.openQuestion) {
      result += `{{open-question:${String(child.attrs.questionId)}}}`;
    }
  });
  return result;
}

function renderBlock(node: ProseMirrorNode): string {
  if (node.type === schema.nodes.paragraph) return renderInline(node);
  if (node.type === schema.nodes.heading) {
    const level = Math.min(6, Math.max(3, Number(node.attrs.level)));
    return `${"#".repeat(level)} ${renderInline(node)}`;
  }
  if (node.type === schema.nodes.codeBlock) {
    return `\`\`\`${String(node.attrs.language)}\n${node.textContent}\n\`\`\``;
  }
  if (node.type === schema.nodes.diagramBlock) {
    return `\`\`\`${String(node.attrs.kind)}\n${String(node.attrs.source)}\n\`\`\``;
  }
  throw new Error(`Cannot render spec node: ${node.type.name}`);
}

export function renderMarkdown(doc: ProseMirrorNode): string {
  const sections: string[] = [];
  doc.forEach((section) => {
    if (section.type !== schema.nodes.section) {
      throw new Error("A spec document can contain only sections");
    }
    const blocks: string[] = [];
    section.forEach((node, _offset, index) => {
      if (index === 0) {
        blocks.push(`## ${renderInline(node)}`);
      } else {
        blocks.push(renderBlock(node));
      }
    });
    sections.push(blocks.join("\n\n"));
  });
  return `${sections.join("\n\n").trimEnd()}\n`;
}

function parseInline(value: string): Fragment {
  const children: ProseMirrorNode[] = [];
  const marker = /\{\{open-question:([^}]+)}}/g;
  let cursor = 0;
  for (const match of value.matchAll(marker)) {
    const start = match.index;
    if (start > cursor) children.push(schema.text(value.slice(cursor, start)));
    children.push(schema.nodes.openQuestion!.create({ questionId: match[1] }));
    cursor = start + match[0].length;
  }
  if (cursor < value.length) children.push(schema.text(value.slice(cursor)));
  return Fragment.fromArray(children);
}

interface MarkdownSection {
  heading: string;
  lines: string[];
}

function splitSections(markdown: string): MarkdownSection[] {
  const sections: MarkdownSection[] = [];
  let current: MarkdownSection | null = null;
  for (const line of markdown.replace(/\r\n/g, "\n").split("\n")) {
    const heading = /^##\s+(.+)$/.exec(line);
    if (heading) {
      current = { heading: heading[1]!, lines: [] };
      sections.push(current);
    } else if (current) {
      current.lines.push(line);
    } else if (line.trim().length > 0) {
      throw new Error("Markdown before the first section heading is not allowed");
    }
  }
  return sections;
}

function parseBlocks(lines: string[]): ProseMirrorNode[] {
  const blocks: ProseMirrorNode[] = [];
  let index = 0;
  while (index < lines.length) {
    if (lines[index]!.trim().length === 0) {
      index += 1;
      continue;
    }
    const fence = /^```([^`]*)$/.exec(lines[index]!);
    if (fence) {
      const language = fence[1]!.trim();
      const source: string[] = [];
      index += 1;
      while (index < lines.length && lines[index] !== "```") {
        source.push(lines[index]!);
        index += 1;
      }
      if (index === lines.length) throw new Error("An open code fence has no closing fence");
      blocks.push(
        schema.nodes.codeBlock!.create(
          { language },
          source.length > 0 ? schema.text(source.join("\n")) : undefined,
        ),
      );
      index += 1;
      continue;
    }
    const heading = /^(#{3,6})\s+(.+)$/.exec(lines[index]!);
    if (heading) {
      blocks.push(
        schema.nodes.heading!.create(
          { level: heading[1]!.length },
          parseInline(heading[2]!),
        ),
      );
      index += 1;
      continue;
    }

    const paragraphLines: string[] = [];
    while (
      index < lines.length &&
      lines[index]!.trim().length > 0 &&
      !/^```/.test(lines[index]!) &&
      !/^#{3,6}\s+/.test(lines[index]!)
    ) {
      paragraphLines.push(lines[index]!);
      index += 1;
    }
    blocks.push(schema.nodes.paragraph!.create(null, parseInline(paragraphLines.join("\n"))));
  }
  return blocks.length > 0 ? blocks : [paragraph()];
}

export function parseMarkdown(markdown: string, template: SpecTemplate): ProseMirrorNode {
  const parsed = splitSections(markdown);
  if (parsed.length !== template.sections.length) {
    throw new Error("Markdown must contain every template section exactly once");
  }

  const sections = template.sections.map((definition, index) => {
    const source = parsed[index]!;
    if (source.heading !== definition.title) {
      throw new Error(`Expected section ${definition.title}, found ${source.heading}`);
    }
    return schema.nodes.section!.create(
      { id: definition.id, templateSectionKey: definition.key },
      [
        schema.nodes.sectionHeading!.create(null, parseInline(source.heading)),
        ...parseBlocks(source.lines),
      ],
    );
  });
  return schema.nodes.doc!.create(null, sections);
}
