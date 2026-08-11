import { describe, expect, test } from "bun:test";
import { prosemirrorToYDoc, yXmlFragmentToProseMirrorRootNode } from "y-prosemirror";

import {
  SPEC_BLOCK_KINDS,
  SPEC_BLOCK_RENDERER_REVISION,
  SPEC_RENDER_TARGETS,
  matchingSpecBlockCachedRender,
  specBlockRegistration,
  specBlockRegistry,
  specRenderTargetRegistration,
  specNodesSemanticallyEqual,
  type SpecBlockAttrs,
} from "./blocks.ts";
import { parseMarkdown, renderMarkdown, schema } from "./schema.ts";

const template = {
  sections: [{ id: "design", key: "design", title: "Design" }],
};

describe("spec block registry", () => {
  test("owns the three version-one block kinds", () => {
    expect(SPEC_BLOCK_KINDS).toEqual(["mermaid", "d2", "flint"]);
    expect(SPEC_RENDER_TARGETS).toEqual(["engrams", "github"]);
    expect(specBlockRegistry.map((entry) => entry.kind)).toEqual(SPEC_BLOCK_KINDS);
    expect(specBlockRegistration("d2")?.label).toBe("D2 diagram");
    expect(specBlockRegistration("future-kind")).toBeNull();
    expect(specRenderTargetRegistration("github").blockModes).toEqual({
      mermaid: "source",
      d2: "cached-render",
      flint: "cached-render",
    });
  });

  test("preserves a source specification through markdown render and parse", () => {
    const source = "flowchart LR\n  A --> B\n  note[``` is source text]\n";
    const block = schema.nodes.diagramBlock!.create({
      id: "request-flow",
      kind: "mermaid",
      source,
      cachedRender: {
        kind: "mermaid",
        source,
        blockId: "request-flow",
        rendererRevision: SPEC_BLOCK_RENDERER_REVISION,
        svg: "<svg><path /></svg>",
      },
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
      cachedRender: {
        kind: "mermaid",
        source: attrs.source,
        blockId: attrs.id,
        rendererRevision: SPEC_BLOCK_RENDERER_REVISION,
        svg: "<svg />",
      },
    });
    const changedSource = schema.nodes.diagramBlock!.create({
      ...attrs,
      source: "flowchart LR\n  A --> C",
      cachedRender: null,
    });

    expect(specNodesSemanticallyEqual(withoutCache, withCache)).toBe(true);
    expect(specNodesSemanticallyEqual(withoutCache, changedSource)).toBe(false);
  });

  test("preserves object cache and provenance values through Yjs", () => {
    const cachedRender = {
      kind: "mermaid",
      source: "flowchart LR\n  A --> B",
      blockId: "request-flow",
      rendererRevision: SPEC_BLOCK_RENDERER_REVISION,
      svg: '<svg><path d="M0 0" /></svg>',
    };
    const provenance = { type: "verified", caption: "Derived from src/server.ts." };
    const block = schema.nodes.diagramBlock!.create({
      id: "request-flow",
      kind: "mermaid",
      source: cachedRender.source,
      cachedRender,
      provenance,
    });
    const document = schema.nodes.doc!.create(null, [
      schema.nodes.section!.create({ id: "design", templateSectionKey: "design" }, [
        schema.nodes.sectionHeading!.create(null, schema.text("Design")),
        block,
      ]),
    ]);

    const ydoc = prosemirrorToYDoc(document, "prosemirror");
    const restored = yXmlFragmentToProseMirrorRootNode(ydoc.getXmlFragment("prosemirror"), schema);

    expect(restored.firstChild?.lastChild?.attrs.cachedRender).toEqual(cachedRender);
    expect(restored.firstChild?.lastChild?.attrs.provenance).toEqual(provenance);
  });

  test("exports live blocks to Engrams and target-aware blocks to GitHub", () => {
    const mermaidSource = "flowchart LR\n  Browser --> API";
    const d2Source = "direction: right\nbrowser -> api";
    const flintSource = '{"data":{"values":[]},"chart_spec":{"chartType":"Bar Chart"}}';
    const document = documentWithBlocks([
      blockAttrs("mermaid-flow", "mermaid", mermaidSource, "<svg>unused mermaid cache</svg>"),
      blockAttrs("d2-flow", "d2", d2Source, "<svg>cached d2</svg>"),
      blockAttrs("flint-chart", "flint", flintSource),
    ]);

    const engrams = renderMarkdown(document, "engrams");
    expect(engrams).toContain(`\`\`\`mermaid\n${mermaidSource}\n\`\`\``);
    expect(engrams).toContain(`\`\`\`d2\n${d2Source}\n\`\`\``);
    expect(engrams).toContain(`\`\`\`flint\n${flintSource}\n\`\`\``);
    expect(engrams).not.toContain("cached d2");

    const github = renderMarkdown(document, "github");
    expect(github).toContain(`\`\`\`mermaid\n${mermaidSource}\n\`\`\``);
    expect(github).toContain("<svg>cached d2</svg>");
    expect(github).not.toContain(`\`\`\`d2\n${d2Source}`);
    expect(github).toContain(`\`\`\`flint\n${flintSource}\n\`\`\``);

    const d2Comment = github.split("\n").find((line) => line.includes('"id":"d2-flow"'));
    expect(d2Comment).toBeDefined();
    expect(JSON.parse(d2Comment!.slice("<!-- spec-block ".length, -" -->".length))).toMatchObject({
      id: "d2-flow",
      kind: "d2",
      source: d2Source,
    });
  });

  test("uses only a cache with the current block identity", () => {
    const attrs = blockAttrs("d2-flow", "d2", "browser -> api", "<svg>cached d2</svg>");
    expect(matchingSpecBlockCachedRender(attrs)?.svg).toBe("<svg>cached d2</svg>");
    for (const cachedRender of [
      { ...attrs.cachedRender!, kind: "flint" },
      { ...attrs.cachedRender!, source: "browser -> worker" },
      { ...attrs.cachedRender!, blockId: "another-flow" },
      { ...attrs.cachedRender!, rendererRevision: "old-revision" },
    ]) {
      const mismatched = { ...attrs, cachedRender };
      expect(matchingSpecBlockCachedRender(mismatched)).toBeNull();
      expect(renderMarkdown(documentWithBlocks([mismatched]), "github")).toContain(
        "```d2\nbrowser -> api\n```",
      );
    }
  });
});

function blockAttrs(id: string, kind: string, source: string, svg?: string): SpecBlockAttrs {
  return {
    id,
    kind,
    source,
    cachedRender:
      svg === undefined
        ? null
        : { kind, source, blockId: id, rendererRevision: SPEC_BLOCK_RENDERER_REVISION, svg },
    provenance: { type: "illustrative" },
  };
}

function documentWithBlocks(blocks: readonly SpecBlockAttrs[]) {
  return schema.nodes.doc!.create(null, [
    schema.nodes.section!.create({ id: "design", templateSectionKey: "design" }, [
      schema.nodes.sectionHeading!.create(null, schema.text("Design")),
      ...blocks.map((attrs) => schema.nodes.diagramBlock!.create(attrs)),
    ]),
  ]);
}
