import {
  Fragment,
  Node as ProseMirrorNode,
  Schema,
  type Mark,
  type NodeSpec,
} from "prosemirror-model";
import { Transform } from "prosemirror-transform";

import {
  isSpecBlockKind,
  matchingSpecBlockCachedRender,
  readSpecBlockAttrs,
  specRenderTargetRegistration,
  type SpecBlockAttrs,
  type SpecBlockProvenance,
  type SpecRenderTarget,
} from "./blocks.ts";

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
  bulletList: {
    content: "listItem+",
    group: "block",
  },
  orderedList: {
    attrs: { start: { default: 1 } },
    content: "listItem+",
    group: "block",
  },
  listItem: {
    content: "paragraph block*",
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
      id: {},
      kind: {},
      source: { default: "" },
      cachedRender: { default: null },
      provenance: { default: { type: "illustrative" } },
    },
    group: "block",
    atom: true,
    isolating: true,
  },
  openQuestion: {
    attrs: {
      questionId: {},
      requestFingerprint: { default: null },
      resolved: { default: false },
      answerMarkdown: { default: null },
    },
    group: "inline",
    inline: true,
    atom: true,
  },
  text: { group: "inline" },
};

/**
 * Inline marks the markdown round-trip understands. The `code` mark excludes
 * the others, so a code span never carries emphasis — exactly the markdown
 * rule.
 */
export const specMarkSpecs = {
  strong: {},
  em: {},
  code: { excludes: "_" },
} as const;

export const schema = new Schema({ nodes: specNodeSpecs, marks: specMarkSpecs });

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

/**
 * Escape the characters that would re-parse as markup. The parser reverses
 * every one of these, so escaped text round-trips instead of accumulating
 * literal backslashes — the defect that taught the agent to write plain prose.
 */
function escapeText(value: string): string {
  return value.replace(/([\\`*_])/g, "\\$1").replace(/\{(?=\{)/g, "\\{");
}

/**
 * Escape a block-construct prefix so a paragraph line never re-parses as one.
 *
 * Every inserted backslash must sit before a character the inline parser
 * unescapes (its PUNCTUATION class), or the escape leaks into the text. A
 * numbered marker therefore escapes its delimiter (`1\.`), never the digit —
 * digits are not punctuation, so `\1.` came back as a literal backslash.
 * Backtick fences need no rule here: a code span renders on one line (see
 * `renderCodeSpan`), and a one-line span whose fence is three-plus backticks
 * always has more backticks later on the line, which the fence-opener grammar
 * (`parseFencedSource`) already rejects.
 */
function escapeLineStarts(value: string): string {
  return value
    .split("\n")
    .map((line) =>
      line
        .replace(/^(\s*)([#]|[-+]\s)/, "$1\\$2")
        .replace(/^(\s*\d{1,9})([.)]\s)/, "$1\\$2"),
    )
    .join("\n");
}

// `em` serializes as `_` so strong-plus-em text emits the unambiguous
// `**_text_**` rather than `***text***`, which the parser cannot split.
const MARK_DELIMITERS: ReadonlyArray<{ name: "strong" | "em"; delimiter: string }> = [
  { name: "strong", delimiter: "**" },
  { name: "em", delimiter: "_" },
];

function renderInline(node: ProseMirrorNode): string {
  let result = "";
  const active: string[] = [];
  const closeTo = (keep: number) => {
    while (active.length > keep) {
      const name = active.pop()!;
      result += MARK_DELIMITERS.find((mark) => mark.name === name)!.delimiter;
    }
  };
  node.forEach((child) => {
    if (child.isText) {
      const text = child.text ?? "";
      const code = child.marks.some((mark) => mark.type === schema.marks.code);
      if (code) {
        closeTo(0);
        result += renderCodeSpan(text);
        return;
      }
      const wanted = MARK_DELIMITERS.filter((mark) =>
        child.marks.some((markInstance) => markInstance.type === schema.marks[mark.name]),
      ).map((mark) => mark.name);
      let shared = 0;
      while (shared < active.length && shared < wanted.length && active[shared] === wanted[shared]) {
        shared += 1;
      }
      closeTo(shared);
      for (const name of wanted.slice(shared)) {
        active.push(name);
        result += MARK_DELIMITERS.find((mark) => mark.name === name)!.delimiter;
      }
      result += escapeText(text);
    } else if (child.type === schema.nodes.openQuestion) {
      if (child.attrs.resolved !== true) {
        closeTo(0);
        result += `{{open-question:${String(child.attrs.questionId)}}}`;
      }
    }
  });
  closeTo(0);
  return result;
}

/** A code span whose fence is longer than any backtick run inside it. */
function renderCodeSpan(rawText: string): string {
  // One line always: a multi-line span would let `escapeLineStarts` (which
  // cannot see span boundaries) insert an escape inside code, and its first
  // line could satisfy the block-fence grammar. CommonMark makes the same
  // normalization; the parser mirrors it, so this is not lossy.
  const text = rawText.replaceAll("\n", " ");
  const longestRun = Math.max(0, ...[...text.matchAll(/`+/g)].map((match) => match[0].length));
  const fence = "`".repeat(Math.max(1, longestRun + 1));
  const pad = longestRun > 0 || text.startsWith("`") || text.endsWith("`") ? " " : "";
  return `${fence}${pad}${text}${pad}${fence}`;
}

function renderListItem(item: ProseMirrorNode, marker: string, target: SpecRenderTarget): string {
  const indent = " ".repeat(marker.length);
  const blocks: string[] = [];
  item.forEach((block) => {
    blocks.push(renderBlock(block, target));
  });
  const body = blocks.join("\n\n");
  return (
    marker +
    body
      .split("\n")
      .map((line, index) => (index === 0 ? line : line.length > 0 ? indent + line : line))
      .join("\n")
  );
}

function renderBlock(node: ProseMirrorNode, target: SpecRenderTarget): string {
  if (node.type === schema.nodes.paragraph) return escapeLineStarts(renderInline(node));
  if (node.type === schema.nodes.heading) {
    const level = Math.min(6, Math.max(3, Number(node.attrs.level)));
    return `${"#".repeat(level)} ${renderInline(node)}`;
  }
  if (node.type === schema.nodes.bulletList) {
    const items: string[] = [];
    node.forEach((item) => items.push(renderListItem(item, "- ", target)));
    return items.join("\n");
  }
  if (node.type === schema.nodes.orderedList) {
    const start = Number(node.attrs.start) || 1;
    const items: string[] = [];
    node.forEach((item, _offset, index) => {
      items.push(renderListItem(item, `${start + index}. `, target));
    });
    return items.join("\n");
  }
  if (node.type === schema.nodes.codeBlock) {
    return renderCodeFence(String(node.attrs.language), node.textContent);
  }
  if (node.type === schema.nodes.diagramBlock) {
    const block = readSpecBlockAttrs(node.attrs);
    const mode = isSpecBlockKind(block.kind)
      ? specRenderTargetRegistration(target).blockModes[block.kind]
      : "source";
    if (mode === "cached-render") {
      const cache = matchingSpecBlockCachedRender(block);
      if (cache) {
        return `${renderBlockMetadata(block, true)}\n${cache.svg}`;
      }
    }
    return `${renderBlockMetadata(block, false)}\n${renderCodeFence(block.kind, block.source)}`;
  }
  throw new Error(`Cannot render spec node: ${node.type.name}`);
}

export function renderMarkdown(doc: ProseMirrorNode, target: SpecRenderTarget = "engrams"): string {
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
        blocks.push(renderBlock(node, target));
      }
    });
    sections.push(blocks.join("\n\n"));
  });
  return `${sections.join("\n\n").trimEnd()}\n`;
}

function renderBlockMetadata(block: SpecBlockAttrs, includeSource: boolean): string {
  const metadata = includeSource
    ? {
        id: block.id,
        kind: block.kind,
        provenance: block.provenance,
        source: block.source,
      }
    : { id: block.id, kind: block.kind, provenance: block.provenance };
  return `<!-- spec-block ${escapeComment(JSON.stringify(metadata))} -->`;
}

interface InlineParseState {
  children: ProseMirrorNode[];
  text: string;
  marks: readonly Mark[];
}

/**
 * Parse inline markdown: `**strong**`, `*em*`/`_em_`, `` `code` ``, backslash
 * escapes, and `{{open-question:id}}` markers. Unmatched delimiters stay
 * literal, and every character `escapeText` escapes is unescaped here — the
 * two halves are a round trip, not a ratchet.
 */
function parseInline(value: string): Fragment {
  const state: InlineParseState = { children: [], text: "", marks: [] };
  parseInlineInto(state, value);
  flushText(state);
  return Fragment.fromArray(state.children);
}

function flushText(state: InlineParseState): void {
  if (state.text.length === 0) return;
  state.children.push(schema.text(state.text, state.marks));
  state.text = "";
}

function withMark(state: InlineParseState, markName: "strong" | "em", inner: string): void {
  flushText(state);
  const before = state.marks;
  state.marks = schema.marks[markName]!.create().addToSet([...before]);
  parseInlineInto(state, inner);
  flushText(state);
  state.marks = before;
}

/**
 * Failed close searches, remembered for the rest of the string.
 *
 * A close search that reaches end-of-string proves no later opener of the
 * same delimiter can close either (its closer would have closed the earlier
 * search). Without this memo — and without consuming a failed run whole — an
 * adversarial string of unclosed delimiters made the scan quadratic: 20 000
 * bare backticks cost ~2×10⁸ comparisons on the orchestrator's one event
 * loop. With it, each delimiter pays for at most one full scan.
 */
interface InlineScanMemo {
  failedCode: Set<number>;
  failedEmphasis: Set<string>;
}

function parseInlineInto(state: InlineParseState, value: string): void {
  const memo: InlineScanMemo = { failedCode: new Set(), failedEmphasis: new Set() };
  let cursor = 0;
  while (cursor < value.length) {
    const char = value[cursor]!;
    if (char === "\\" && cursor + 1 < value.length && PUNCTUATION.test(value[cursor + 1]!)) {
      state.text += value[cursor + 1]!;
      cursor += 2;
      continue;
    }
    if (char === "{" && value.startsWith("{{open-question:", cursor)) {
      const end = value.indexOf("}}", cursor);
      if (end > cursor) {
        flushText(state);
        state.children.push(
          schema.nodes.openQuestion!.create({
            questionId: value.slice(cursor + "{{open-question:".length, end),
          }),
        );
        cursor = end + 2;
        continue;
      }
    }
    if (char === "`") {
      const run = runLength(value, cursor, "`");
      const close = findCodeClose(value, cursor + run, run, memo);
      if (close >= 0) {
        flushText(state);
        let code = value.slice(cursor + run, close).replaceAll("\n", " ");
        if (code.startsWith(" ") && code.endsWith(" ") && code.trim().length > 0) {
          code = code.slice(1, -1);
        }
        if (code.length > 0) {
          state.children.push(schema.text(code, [schema.marks.code!.create()]));
        }
        cursor = close + run;
        continue;
      }
      // No close anywhere ahead: the whole run is literal. Consuming it one
      // character at a time re-ran the search per character — O(run²).
      state.text += value.slice(cursor, cursor + run);
      cursor += run;
      continue;
    }
    if (char === "*" || char === "_") {
      const run = runLength(value, cursor, char);
      const spans = emphasisSpans(value, cursor, run, memo);
      if (spans) {
        withMark(state, spans.mark, value.slice(spans.innerFrom, spans.innerTo));
        cursor = spans.nextCursor;
        continue;
      }
      state.text += value.slice(cursor, cursor + run);
      cursor += run;
      continue;
    }
    state.text += char;
    cursor += 1;
  }
}

interface EmphasisSpan {
  mark: "strong" | "em";
  innerFrom: number;
  innerTo: number;
  nextCursor: number;
}

/** Try the two-character delimiter first, then the single, so `**a*` still
 *  yields emphasis instead of turning the whole run literal. */
function emphasisSpans(
  value: string,
  cursor: number,
  run: number,
  memo: InlineScanMemo,
): EmphasisSpan | null {
  const char = value[cursor]!;
  if (run >= 2) {
    const delimiter = char.repeat(2);
    const close = findEmphasisClose(value, cursor + 2, delimiter, memo);
    if (close >= 0) {
      return { mark: "strong", innerFrom: cursor + 2, innerTo: close, nextCursor: close + 2 };
    }
  }
  const close = findEmphasisClose(value, cursor + 1, char, memo);
  if (close >= 0) {
    return { mark: "em", innerFrom: cursor + 1, innerTo: close, nextCursor: close + 1 };
  }
  return null;
}

const PUNCTUATION = /[!-/:-@[-`{-~]/;

function runLength(value: string, index: number, char: string): number {
  let end = index;
  while (end < value.length && value[end] === char) end += 1;
  return end - index;
}

/** The close of a code span: the next run of exactly the opening length. */
function findCodeClose(value: string, from: number, run: number, memo: InlineScanMemo): number {
  if (memo.failedCode.has(run)) return -1;
  let cursor = from;
  while (cursor < value.length) {
    if (value[cursor] === "`") {
      const length = runLength(value, cursor, "`");
      if (length === run) return cursor;
      cursor += length;
    } else {
      cursor += 1;
    }
  }
  memo.failedCode.add(run);
  return -1;
}

/** The close of an emphasis span: non-empty, no spaces hugging the delimiters. */
function findEmphasisClose(
  value: string,
  from: number,
  delimiter: string,
  memo: InlineScanMemo,
): number {
  if (memo.failedEmphasis.has(delimiter)) return -1;
  if (from >= value.length || value[from] === " ") return -1;
  let cursor = from;
  while (cursor < value.length) {
    if (value[cursor] === "\\") {
      cursor += 2;
      continue;
    }
    if (value[cursor] === "`") {
      const run = runLength(value, cursor, "`");
      const close = findCodeClose(value, cursor + run, run, memo);
      cursor = close >= 0 ? close + run : cursor + run;
      continue;
    }
    if (value.startsWith(delimiter, cursor)) {
      const more = runLength(value, cursor, delimiter[0]!);
      if (more === delimiter.length && cursor > from && value[cursor - 1] !== " ") return cursor;
      cursor += more;
      continue;
    }
    cursor += 1;
  }
  // The scan reached the end: no valid closer exists after `from`, so none
  // exists for any later opener of this delimiter either.
  memo.failedEmphasis.add(delimiter);
  return -1;
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

const LIST_MARKER = /^(\s*)([-+*]|\d{1,9}[.)])\s+(.*)$/;

function parseBlocks(lines: string[]): ProseMirrorNode[] {
  const blocks: ProseMirrorNode[] = [];
  let index = 0;
  while (index < lines.length) {
    if (lines[index]!.trim().length === 0) {
      index += 1;
      continue;
    }
    const blockMetadata = parseBlockMetadata(lines[index]!);
    if (blockMetadata) {
      const parsed = parseFencedSource(lines, index + 1);
      if (!parsed) throw new Error("A spec block must have a fenced source specification");
      if (parsed.language !== blockMetadata.kind) {
        throw new Error("A spec block fence must use its registered kind");
      }
      blocks.push(
        schema.nodes.diagramBlock!.create({
          id: blockMetadata.id,
          kind: blockMetadata.kind,
          source: parsed.source,
          cachedRender: null,
          provenance: blockMetadata.provenance,
        }),
      );
      index = parsed.nextIndex;
      continue;
    }
    const fence = parseFencedSource(lines, index);
    if (fence) {
      blocks.push(
        schema.nodes.codeBlock!.create(
          { language: fence.language },
          fence.source.length > 0 ? schema.text(fence.source) : undefined,
        ),
      );
      index = fence.nextIndex;
      continue;
    }
    // A section body may use any heading depth; the document clamps to h3+
    // because h2 is the section boundary.
    const heading = /^(#{1,6})\s+(.+)$/.exec(lines[index]!);
    if (heading) {
      blocks.push(
        schema.nodes.heading!.create(
          { level: Math.max(3, heading[1]!.length) },
          parseInline(heading[2]!),
        ),
      );
      index += 1;
      continue;
    }
    if (LIST_MARKER.test(lines[index]!)) {
      const list = parseList(lines, index);
      blocks.push(list.node);
      index = list.nextIndex;
      continue;
    }

    // The current line failed every block form above, so it is paragraph text
    // no matter what it starts with. Consuming it unconditionally guarantees
    // progress: a line such as "``` a``b ``` more" opens no fence (backticks
    // follow on the same line) yet also matched this loop's fence guard — the
    // parser pushed empty paragraphs forever without ever advancing.
    const paragraphLines: string[] = [lines[index]!];
    index += 1;
    while (
      index < lines.length &&
      lines[index]!.trim().length > 0 &&
      !/^`{3,}/.test(lines[index]!) &&
      !/^#{1,6}\s+/.test(lines[index]!) &&
      !LIST_MARKER.test(lines[index]!)
    ) {
      paragraphLines.push(lines[index]!);
      index += 1;
    }
    blocks.push(schema.nodes.paragraph!.create(null, parseInline(paragraphLines.join("\n"))));
  }
  return blocks.length > 0 ? blocks : [paragraph()];
}

interface ParsedList {
  node: ProseMirrorNode;
  nextIndex: number;
}

/**
 * Parse one list whose items sit at the indent of the first marker. Lines
 * indented past a marker continue that item and are parsed recursively, so
 * nested lists work. The list ends at a dedent or at a blank line followed by
 * a non-list line.
 */
function parseList(lines: string[], start: number): ParsedList {
  const first = LIST_MARKER.exec(lines[start]!)!;
  const indent = first[1]!.length;
  const ordered = /\d/.test(first[2]![0]!);
  const items: ProseMirrorNode[] = [];
  let index = start;
  while (index < lines.length) {
    const line = lines[index]!;
    if (line.trim().length === 0) {
      // A blank line ends the list unless another item (or continuation)
      // of this same list follows.
      const next = lines[index + 1];
      if (next === undefined) break;
      const marker = LIST_MARKER.exec(next);
      const continues =
        (marker !== null && marker[1]!.length >= indent) ||
        (next.trim().length > 0 && leadingSpaces(next) > indent);
      if (!continues) break;
      index += 1;
      continue;
    }
    const marker = LIST_MARKER.exec(line);
    if (!marker || marker[1]!.length !== indent) break;
    // A marker of the other kind starts a new list, not a new item.
    if (/\d/.test(marker[2]![0]!) !== ordered) break;
    const itemIndent = indent + marker[2]!.length + 1;
    const content: string[] = [marker[3]!];
    index += 1;
    while (index < lines.length) {
      const candidate = lines[index]!;
      if (candidate.trim().length === 0) {
        const next = lines[index + 1];
        if (next !== undefined && next.trim().length > 0 && leadingSpaces(next) >= itemIndent) {
          content.push("");
          index += 1;
          continue;
        }
        break;
      }
      if (leadingSpaces(candidate) < itemIndent) break;
      content.push(candidate.slice(itemIndent));
      index += 1;
    }
    items.push(schema.nodes.listItem!.create(null, parseBlocks(content)));
  }
  const node = ordered
    ? schema.nodes.orderedList!.create(
        { start: Number.parseInt(first[2]!, 10) || 1 },
        items,
      )
    : schema.nodes.bulletList!.create(null, items);
  return { node, nextIndex: index };
}

function leadingSpaces(line: string): number {
  return line.length - line.trimStart().length;
}

interface ParsedFence {
  language: string;
  source: string;
  nextIndex: number;
}

function renderCodeFence(language: string, source: string): string {
  const longestRun = Math.max(0, ...[...source.matchAll(/`+/g)].map((match) => match[0].length));
  const fence = "`".repeat(Math.max(3, longestRun + 1));
  return `${fence}${language}\n${source}\n${fence}`;
}

function parseFencedSource(lines: string[], index: number): ParsedFence | null {
  const opening = /^(`{3,})([^`]*)$/.exec(lines[index] ?? "");
  if (!opening) return null;
  const fence = opening[1]!;
  const source: string[] = [];
  let cursor = index + 1;
  while (cursor < lines.length && lines[cursor] !== fence) {
    source.push(lines[cursor]!);
    cursor += 1;
  }
  if (cursor === lines.length) throw new Error("An open code fence has no closing fence");
  return {
    language: opening[2]!.trim(),
    source: source.join("\n"),
    nextIndex: cursor + 1,
  };
}

interface BlockMetadata {
  id: string;
  kind: string;
  provenance: SpecBlockProvenance;
}

function parseBlockMetadata(line: string): BlockMetadata | null {
  const match = /^<!-- spec-block (.+) -->$/.exec(line);
  if (!match) return null;
  let value: unknown;
  try {
    value = JSON.parse(match[1]!);
  } catch {
    throw new Error("A spec block has invalid metadata");
  }
  const block = readSpecBlockAttrs(isRecord(value) ? value : {});
  if (block.id === "unknown" || block.kind === "unknown") {
    throw new Error("A spec block must have an id and kind");
  }
  return { id: block.id, kind: block.kind, provenance: block.provenance };
}

function escapeComment(value: string): string {
  return value.replaceAll("--", "\\u002d\\u002d");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function parseMarkdownBlocks(markdown: string): ProseMirrorNode[] {
  return parseBlocks(markdown.replace(/\r\n/g, "\n").split("\n"));
}

/**
 * Parse a section body, dropping a leading heading that repeats the section
 * title. The canvas renders the stable section heading itself, so a body that
 * opens with `## Goals` (or `**Goals**` alone) would show the title twice —
 * the first live drive produced exactly that in every seeded section.
 */
export function parseSectionBody(markdown: string, sectionTitle: string): ProseMirrorNode[] {
  const lines = markdown.replace(/\r\n/g, "\n").split("\n");
  let start = 0;
  while (start < lines.length && lines[start]!.trim().length === 0) start += 1;
  const first = lines[start]?.trim() ?? "";
  const title = sectionTitle.trim().toLowerCase();
  const heading = /^#{1,6}\s+(.+?)\s*$/.exec(first);
  const bold = /^\*\*(.+?)\*\*$/.exec(first);
  const repeated = (heading?.[1] ?? bold?.[1])?.trim().toLowerCase();
  if (repeated !== undefined && repeated === title) {
    return parseBlocks(lines.slice(start + 1));
  }
  return parseBlocks(lines);
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
