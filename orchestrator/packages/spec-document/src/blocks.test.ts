import { describe, expect, test } from "bun:test";

import {
  SPEC_BLOCK_KINDS,
  specBlockRegistration,
  specBlockRegistry,
  specNodesSemanticallyEqual,
} from "./blocks.ts";
import { parseMarkdown, renderMarkdown, schema } from "./schema.ts";

const template = {
  sections: [{ id: "design", key: "design", title: "Design" }],
};

describe("spec block registry", () => {
  test("owns the three version-one block kinds", () => {
    expect(SPEC_BLOCK_KINDS).toEqual(["mermaid", "d2", "flint"]);
    expect(specBlockRegistry.map((entry) => entry.kind)).toEqual(SPEC_BLOCK_KINDS);
    expect(specBlockRegistration("d2")?.label).toBe("D2 diagram");
    expect(specBlockRegistration("future-kind")).toBeNull();
  });

  test("preserves a source specification through markdown render and parse", () => {
    const source = "flowchart LR\n  A --> B\n  note[``` is source text]\n";
    const block = schema.nodes.diagramBlock!.create({
      id: "request-flow",
      kind: "mermaid",
      source,
      cachedRender: { kind: "mermaid", source, svg: "<svg><path /></svg>" },
      provenance: { type: "verified", caption: "Derived from src/server.ts." },
    });
    const document = schema.nodes.doc!.create(null, [
      schema.nodes.section!.create({ id: "design", templateSectionKey: "design" }, [
        schema.nodes.sectionHeading!.create(null, schema.text("Design")),
        block,
      ]),
    ]);

    const markdown = renderMarkdown(document);
    const parsed = parseMarkdown(markdown, template).firstChild?.lastChild;

    expect(markdown).toContain("<!-- spec-block");
    expect(markdown).not.toContain("<svg>");
    expect(parsed?.attrs).toEqual({
      id: "request-flow",
      kind: "mermaid",
      source,
      cachedRender: null,
      provenance: { type: "verified", caption: "Derived from src/server.ts." },
    });
  });

  test("keeps an unknown product kind parseable", () => {
    const markdown = `## Design

<!-- spec-block {"id":"legacy","kind":"legacy-chart","provenance":{"type":"illustrative"}} -->
\`\`\`legacy-chart
value: 1
\`\`\`
`;

    const parsed = parseMarkdown(markdown, template).firstChild?.lastChild;
    expect(parsed?.attrs.kind).toBe("legacy-chart");
    expect(parsed?.attrs.source).toBe("value: 1");
  });

  test("excludes a cached render from semantic comparison", () => {
    const attrs = {
      id: "request-flow",
      kind: "mermaid",
      source: "flowchart LR\n  A --> B",
      provenance: { type: "illustrative" },
    };
    const withoutCache = schema.nodes.diagramBlock!.create({ ...attrs, cachedRender: null });
    const withCache = schema.nodes.diagramBlock!.create({
      ...attrs,
      cachedRender: { kind: "mermaid", source: attrs.source, svg: "<svg />" },
    });
    const changedSource = schema.nodes.diagramBlock!.create({
      ...attrs,
      source: "flowchart LR\n  A --> C",
      cachedRender: null,
    });

    expect(specNodesSemanticallyEqual(withoutCache, withCache)).toBe(true);
    expect(specNodesSemanticallyEqual(withoutCache, changedSource)).toBe(false);
  });
});
