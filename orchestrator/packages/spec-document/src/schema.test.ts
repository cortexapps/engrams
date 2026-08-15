import { describe, expect, test } from "bun:test";

import {
  parseMarkdownBlocks,
  parseSectionBody,
  renderMarkdown,
  schema,
} from "./schema.ts";
import type { Node as ProseMirrorNode } from "prosemirror-model";

function sectionDocument(blocks: ProseMirrorNode[]): ProseMirrorNode {
  return schema.nodes.doc!.create(null, [
    schema.nodes.section!.create({ id: "section-1", templateSectionKey: "problem" }, [
      schema.nodes.sectionHeading!.create(null, schema.text("Problem")),
      ...blocks,
    ]),
  ]);
}

/** Render a body to markdown and parse it back; the body must survive. */
function roundTrip(blocks: ProseMirrorNode[]): ProseMirrorNode[] {
  const markdown = renderMarkdown(sectionDocument(blocks));
  const body = markdown.replace(/^## Problem\n+/, "");
  return parseMarkdownBlocks(body);
}

function expectRoundTrip(blocks: ProseMirrorNode[]): void {
  const parsed = roundTrip(blocks);
  const before = sectionDocument(blocks);
  const after = sectionDocument(parsed);
  expect(after.toJSON()).toEqual(before.toJSON());
}

const paragraph = (...content: ProseMirrorNode[]) =>
  schema.nodes.paragraph!.create(null, content);
const text = (value: string, markNames: Array<"strong" | "em" | "code"> = []) =>
  schema.text(
    value,
    markNames.map((name) => schema.marks[name]!.create()),
  );

describe("markdown round-trip", () => {
  test("bold, italic, and code marks survive", () => {
    expectRoundTrip([
      paragraph(
        text("The premise is "),
        text("right", ["strong"]),
        text(" but "),
        text("only half", ["em"]),
        text(" of "),
        text("jv_load_file", ["code"]),
        text(" is involved."),
      ),
    ]);
  });

  test("bold italic text uses the splittable form", () => {
    const markdown = renderMarkdown(
      sectionDocument([paragraph(text("both", ["strong", "em"]))]),
    );
    expect(markdown).toContain("**_both_**");
    expectRoundTrip([paragraph(text("both", ["strong", "em"]))]);
  });

  test("agent markdown parses into marks instead of literal characters", () => {
    const [block] = parseMarkdownBlocks("The **premise** is `jv_load_file` at *last*.");
    expect(block!.type.name).toBe("paragraph");
    const rendered = renderMarkdown(sectionDocument([block!]));
    expect(rendered).not.toContain("\\*");
    expect(rendered).toContain("**premise**");
    expect(rendered).toContain("`jv_load_file`");
  });

  test("escaped characters round-trip instead of accumulating backslashes", () => {
    const literal = paragraph(text("a *literal* star and a `backtick`"));
    // Not marks: the source node is plain text, so the render escapes and the
    // parse must unescape back to the same plain text.
    const once = renderMarkdown(sectionDocument([literal]));
    const twice = renderMarkdown(sectionDocument(roundTrip([literal])));
    expect(twice).toEqual(once);
  });

  test("bullet and ordered lists round-trip", () => {
    const list = schema.nodes.bulletList!.create(null, [
      schema.nodes.listItem!.create(null, [paragraph(text("first item"))]),
      schema.nodes.listItem!.create(null, [paragraph(text("second "), text("bold", ["strong"]))]),
    ]);
    const ordered = schema.nodes.orderedList!.create({ start: 3 }, [
      schema.nodes.listItem!.create(null, [paragraph(text("third"))]),
      schema.nodes.listItem!.create(null, [paragraph(text("fourth"))]),
    ]);
    expectRoundTrip([list, ordered]);
  });

  test("numbered markdown becomes an ordered list", () => {
    const blocks = parseMarkdownBlocks(
      ["1. A too-large input fails with a diagnostic.", "2. The limit bounds memory."].join("\n"),
    );
    expect(blocks).toHaveLength(1);
    expect(blocks[0]!.type.name).toBe("orderedList");
    expect(blocks[0]!.childCount).toBe(2);
  });

  test("nested lists parse and round-trip", () => {
    const blocks = parseMarkdownBlocks(
      ["- outer", "  - inner one", "  - inner two", "- outer two"].join("\n"),
    );
    expect(blocks).toHaveLength(1);
    const outer = blocks[0]!;
    expect(outer.type.name).toBe("bulletList");
    expect(outer.childCount).toBe(2);
    expect(outer.firstChild!.childCount).toBe(2);
    expect(outer.firstChild!.lastChild!.type.name).toBe("bulletList");
    expectRoundTrip(blocks);
  });

  test("a body h2 becomes an h3 rather than literal text", () => {
    const blocks = parseMarkdownBlocks("## What the repository shows\n\nBody text.");
    expect(blocks[0]!.type.name).toBe("heading");
    expect(blocks[0]!.attrs.level).toBe(3);
  });

  test("unmatched delimiters stay literal", () => {
    const blocks = parseMarkdownBlocks("a lone * asterisk and `an open backtick");
    expect(blocks[0]!.textContent).toBe("a lone * asterisk and `an open backtick");
  });

  test("open-question markers survive inside marked text", () => {
    const blocks = parseMarkdownBlocks(
      "Before {{open-question:2a3a8e18-0000-4000-8000-000000000000}} after.",
    );
    let found = 0;
    for (const block of blocks) {
      block.descendants((node) => {
        if (node.type === schema.nodes.openQuestion) found += 1;
        return true;
      });
    }
    expect(found).toBe(1);
    expectRoundTrip(blocks);
  });

  test("code blocks keep their fences", () => {
    expectRoundTrip([
      schema.nodes.codeBlock!.create({ language: "go" }, schema.text("func main() {}")),
    ]);
  });

  test("a paragraph that starts with a numbered marker round-trips without a stray backslash", () => {
    // The line-start escape must sit before punctuation the inline parser
    // unescapes. Escaping the digit (`\1.`) leaked the backslash into the
    // text, because digits are not in the unescape class.
    expectRoundTrip([paragraph(text("1. A numbered opening that is prose, not a list."))]);
    expectRoundTrip([paragraph(text("42) Another prose opener."))]);
  });

  test("a leading code span with a doubled backtick round-trips", () => {
    // Renders as ``` a``b ``` at line start. The old fence escape prepended
    // a backslash that consumed one backtick on reparse and split the span.
    expectRoundTrip([
      paragraph(text("a``b", ["code"]), text(" ends the span.")),
    ]);
  });

  test("newlines inside a code span normalize to spaces, both directions", () => {
    const [block] = parseMarkdownBlocks("`line one\nline two` after");
    expect(block!.firstChild!.text).toBe("line one line two");
    const rendered = renderMarkdown(
      sectionDocument([paragraph(text("line one\nline two", ["code"]))]),
    );
    expect(rendered).toContain("`line one line two`");
  });

  test("a backtick-led line that opens no fence parses as text and terminates", () => {
    // This line starts with ``` but is not a fence opener (backticks follow
    // on the same line). The block loop used to exclude it from paragraph
    // collection too, consuming nothing and pushing empty paragraphs forever.
    const blocks = parseMarkdownBlocks("``` a``b ``` ends the span.");
    expect(blocks).toHaveLength(1);
    expect(blocks[0]!.type.name).toBe("paragraph");
    expect(blocks[0]!.firstChild!.text).toBe("a``b");
  });

  test("an unclosed code fence is refused, quickly", () => {
    const startedAt = performance.now();
    expect(() => parseMarkdownBlocks("```go\n" + "`".repeat(20_000))).toThrow(
      "An open code fence has no closing fence",
    );
    expect(performance.now() - startedAt).toBeLessThan(1_000);
  });

  test("unclosed delimiter floods parse in linear time", () => {
    // A crafted 20k-character run of unclosed delimiters made the old scan
    // quadratic (~2×10⁸ comparisons) and pinned the event loop. The bound is
    // generous for a linear scan and hopeless for a quadratic one. The "x "
    // prefix keeps each flood on the inline path (a bare backtick flood at
    // line start is an unclosed block fence, refused above).
    for (const flood of [
      "x " + "`".repeat(20_000),
      "x " + "*".repeat(20_000),
      "x " + "_a".repeat(10_000),
      "x " + "``a`a".repeat(4_000),
      ("``` a``b ``` x\n").repeat(2_000),
    ]) {
      const startedAt = performance.now();
      const blocks = parseMarkdownBlocks(flood);
      expect(performance.now() - startedAt).toBeLessThan(1_000);
      expect(blocks.length).toBeGreaterThan(0);
    }
  });
});

describe("parseSectionBody", () => {
  test("drops a leading heading that repeats the section title", () => {
    const blocks = parseSectionBody("## Goals\n\nFirst real paragraph.", "Goals");
    expect(blocks).toHaveLength(1);
    expect(blocks[0]!.type.name).toBe("paragraph");
    expect(blocks[0]!.textContent).toBe("First real paragraph.");
  });

  test("drops a bold line that repeats the section title", () => {
    const blocks = parseSectionBody("**Goals**\n\nFirst real paragraph.", "Goals");
    expect(blocks).toHaveLength(1);
    expect(blocks[0]!.textContent).toBe("First real paragraph.");
  });

  test("keeps a heading that says something else", () => {
    const blocks = parseSectionBody("### What the repository shows\n\nBody.", "Goals");
    expect(blocks[0]!.type.name).toBe("heading");
  });

  test("keeps a plain first line even when it matches the title", () => {
    const blocks = parseSectionBody("Goals are stated below.", "Goals");
    expect(blocks[0]!.textContent).toBe("Goals are stated below.");
  });
});
