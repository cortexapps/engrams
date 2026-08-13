import { afterEach, describe, expect, test, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import {
  matchingSpecBlockCachedRender,
  SPEC_BLOCK_RENDERER_REVISION,
  type SpecBlockAttrs,
} from "@engrams/spec-document";

import { SpecBlockView } from "./SpecBlock";
import { renderSpecBlock, sanitizeSvg, specBlockRenderAdapters } from "./block-renderers";

const canvasContextDescriptor = Object.getOwnPropertyDescriptor(
  HTMLCanvasElement.prototype,
  "getContext",
);

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  Reflect.deleteProperty(SVGElement.prototype, "getBBox");
  Reflect.deleteProperty(SVGElement.prototype, "getComputedTextLength");
  if (canvasContextDescriptor) {
    Object.defineProperty(HTMLCanvasElement.prototype, "getContext", canvasContextDescriptor);
  }
});

describe("spec blocks", () => {
  test("renders a Mermaid block without a network request", async () => {
    const fetch = vi.fn(() => Promise.reject(new Error("Network access is not allowed.")));
    vi.stubGlobal("fetch", fetch);
    Object.defineProperty(SVGElement.prototype, "getBBox", {
      configurable: true,
      value: () => ({ x: 0, y: 0, width: 100, height: 20 }),
    });
    Object.defineProperty(SVGElement.prototype, "getComputedTextLength", {
      configurable: true,
      value: () => 100,
    });
    Object.defineProperty(HTMLCanvasElement.prototype, "getContext", {
      configurable: true,
      value: () => ({ measureText: () => ({ width: 100 }) }),
    });

    const onCache = vi.fn();
    render(
      <SpecBlockView attrs={attrs({ source: "flowchart LR\n  A --> B" })} onCache={onCache} />,
    );

    const diagram = await screen.findByRole("img", { name: "Mermaid diagram" }, { timeout: 5_000 });
    expect(diagram.querySelector("svg")).not.toBeNull();
    expect(fetch).not.toHaveBeenCalled();
    await waitFor(() => expect(onCache).toHaveBeenCalledOnce());
    expect(onCache.mock.calls[0]?.[0]).toMatchObject({ source: "flowchart LR\n  A --> B" });
    expect(onCache.mock.calls[0]?.[0]).toMatchObject({
      blockId: "request-flow",
      rendererRevision: SPEC_BLOCK_RENDERER_REVISION,
    });
  });

  test("uses a matching self-contained cache without rendering again", async () => {
    const fetch = vi.fn(() => Promise.reject(new Error("Network access is not allowed.")));
    vi.stubGlobal("fetch", fetch);
    const source = "flowchart LR\n  A --> B";

    render(
      <SpecBlockView
        attrs={attrs({
          source,
          cachedRender: {
            kind: "mermaid",
            source,
            blockId: "request-flow",
            rendererRevision: SPEC_BLOCK_RENDERER_REVISION,
            svg: '<svg xmlns="http://www.w3.org/2000/svg"><text>Cached</text></svg>',
          },
        })}
      />,
    );

    expect(await screen.findByText("Cached")).toBeTruthy();
    expect(fetch).not.toHaveBeenCalled();
  });

  test("shows an unknown kind as a labeled code block", () => {
    render(<SpecBlockView attrs={attrs({ kind: "future-diagram", source: "actor -> service" })} />);

    expect(screen.getByText("Unknown block: future-diagram")).toBeTruthy();
    expect(screen.getByLabelText("Unsupported block kind: future-diagram").textContent).toBe(
      "actor -> service",
    );
  });

  test("shows verified and illustrative blocks in separate registers", () => {
    const { container, rerender } = render(
      <SpecBlockView
        attrs={attrs({
          cachedRender: cached("flowchart LR\n  A --> B"),
          provenance: { type: "verified", caption: "Derived from src/server.ts." },
        })}
      />,
    );
    expect(container.querySelector('[data-provenance="verified"]')).not.toBeNull();
    expect(screen.getByText("Verified")).toBeTruthy();
    expect(screen.getByText("Derived from src/server.ts.")).toBeTruthy();

    rerender(
      <SpecBlockView
        attrs={attrs({
          cachedRender: cached("flowchart LR\n  A --> B"),
          provenance: { type: "illustrative", caption: "Proposed request flow." },
        })}
      />,
    );
    expect(container.querySelector('[data-provenance="illustrative"]')).not.toBeNull();
    expect(screen.getByText("Illustrative")).toBeTruthy();
  });

  test("invalidates caches for another block or renderer revision", async () => {
    const renderMermaid = vi
      .spyOn(specBlockRenderAdapters.mermaid, "render")
      .mockResolvedValue('<svg xmlns="http://www.w3.org/2000/svg"><text>Fresh render</text></svg>');
    const source = "flowchart LR\n  A --> B";
    const { rerender } = render(
      <SpecBlockView
        attrs={attrs({
          source,
          cachedRender: {
            ...cached(source),
            blockId: "another-block",
          },
        })}
      />,
    );

    expect(await screen.findByText("Fresh render")).toBeTruthy();
    expect(renderMermaid).toHaveBeenCalledOnce();

    renderMermaid.mockClear();
    rerender(
      <SpecBlockView
        attrs={attrs({
          id: "second-block",
          source,
          cachedRender: {
            ...cached(source),
            blockId: "second-block",
            rendererRevision: "old-renderer",
          },
        })}
      />,
    );
    await waitFor(() => expect(renderMermaid).toHaveBeenCalledOnce());
  });

  test("checks every cache identity field before live rendering uses it", () => {
    const source = "flowchart LR\n  A --> B";
    const current = attrs({ source, cachedRender: cached(source) });
    expect(matchingSpecBlockCachedRender(current)).not.toBeNull();
    expect(
      [
        { ...current.cachedRender!, kind: "d2" },
        { ...current.cachedRender!, source: "flowchart LR\n  A --> C" },
        { ...current.cachedRender!, blockId: "another-block" },
        { ...current.cachedRender!, rendererRevision: "old-renderer" },
      ].every(
        (cachedRender) => matchingSpecBlockCachedRender({ ...current, cachedRender }) === null,
      ),
    ).toBe(true);
  });

  test("uses a closed SVG allowlist for active and network-capable content", () => {
    expect(Object.keys(specBlockRenderAdapters)).toEqual(["mermaid", "d2", "flint"]);
    const sanitized = sanitizeSvg(
      '<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" onload="alert(1)">' +
        "<script>alert(1)</script><foreignObject><div>foreign</div></foreignObject>" +
        '<animate attributeName="href" to="https://example.com/a" />' +
        '<set attributeName="fill" to="url(https://example.com/a.svg)" />' +
        "<style>@import url(https://example.com/a.css); .x{fill:red}</style>" +
        '<a href="https://example.com"><text>Unlinked</text></a>' +
        '<image href="data:image/png;base64,AAAA" />' +
        '<rect fill="url(https://example.com/a.svg)" clip-path="url(https://example.com/c.svg)" ' +
        'style="fill: u\\72l(https://example.com/b.svg)" />' +
        '<path id="safe-path" d="M0 0" marker-end="url(#safe-marker)" />' +
        '<use href="#safe-path" xlink:href="https://example.com/b.svg#x" />' +
        "</svg>",
      "request-flow",
    );
    const svg = new DOMParser().parseFromString(sanitized, "image/svg+xml");
    expect(svg.querySelector("script, foreignObject, animate, set, style, a, image")).toBeNull();
    expect(svg.querySelector("text")?.textContent).toBe("Unlinked");
    expect(svg.querySelector("rect")?.attributes).toHaveLength(0);
    expect(svg.documentElement.hasAttribute("onload")).toBe(false);
    const pathId = svg.querySelector("path")?.id;
    expect(pathId).toMatch(/^spec-block-/);
    expect(pathId).not.toBe("safe-path");
    expect(svg.querySelector("path")?.getAttribute("marker-end")).toMatch(
      /^url\(#spec-block-.*-safe-marker\)$/,
    );
    expect(svg.querySelector("use")?.getAttribute("href")).toBe(`#${pathId}`);
    expect(svg.querySelector("use")?.hasAttribute("xlink:href")).toBe(false);
    expect(sanitizeSvg(sanitized, "request-flow")).toBe(sanitized);
  });

  test("keeps a Mermaid theme stylesheet scoped to the diagram's own root id", () => {
    const sanitized = sanitizeSvg(
      '<svg xmlns="http://www.w3.org/2000/svg" id="spec-mermaid-flow">' +
        "<style>" +
        "#spec-mermaid-flow{font-family:'trebuchet ms',verdana;font-size:16px;fill:#333;}" +
        "#spec-mermaid-flow .node rect{fill:#ececff;stroke:#9370db;stroke-width:1px;}" +
        "#spec-mermaid-flow .edgeLabel{background-color:hsl(80, 100%, 96%);text-align:center;fill:#333;}" +
        "#spec-mermaid-flow .arrow{fill:url(#spec-mermaid-flow-grad);}" +
        "</style>" +
        '<rect class="node" />' +
        "</svg>",
      "request-flow",
    );
    const svg = new DOMParser().parseFromString(sanitized, "image/svg+xml");
    const css = svg.querySelector("style")?.textContent ?? "";
    const rootId = svg.documentElement.id;
    expect(rootId).toMatch(/^spec-block-.*spec-mermaid-flow$/);
    // Every kept selector anchors on the PREFIXED root id.
    expect(css).toContain(`#${rootId} .node rect { fill: #ececff; stroke: #9370db`);
    expect(css).toContain(`#${rootId} { font-family: 'trebuchet ms',verdana`);
    // Unlisted properties drop without costing the rule its allowed paint.
    expect(css).not.toContain("background-color");
    expect(css).not.toContain("text-align");
    expect(css).toContain(`#${rootId} .edgeLabel { fill: #333; }`);
    // url(#…) fragments inside declarations are re-namespaced.
    expect(css).toMatch(/\.arrow \{ fill: url\(#spec-block-.*-spec-mermaid-flow-grad\); \}/);
    // A second pass is a fixed point.
    expect(sanitizeSvg(sanitized, "request-flow")).toBe(sanitized);
  });

  test("drops stylesheet rules that could reach outside the block", () => {
    const scoped = (body: string) =>
      sanitizeSvg(
        `<svg xmlns="http://www.w3.org/2000/svg" id="m"><style>${body}</style><rect /></svg>`,
        "request-flow",
      );
    // Unanchored selectors go, anchored siblings stay.
    const mixed = new DOMParser().parseFromString(
      scoped("body{display:none;}#m rect{fill:red;}.sidebar{opacity:0;}#other #m{fill:red;}"),
      "image/svg+xml",
    );
    const mixedCss = mixed.querySelector("style")?.textContent ?? "";
    expect(mixedCss).toContain("rect { fill: red; }");
    expect(mixedCss).not.toContain("display");
    expect(mixedCss).not.toContain("sidebar");
    expect(mixedCss).not.toContain("#other");
    // An at-rule, an escape, a comment, or unbalanced braces drop the sheet.
    for (const hostile of [
      "@media screen{#m rect{fill:red;}}",
      "@import url(x);#m rect{fill:red;}",
      "#m rect{fill:\\72 ed;}",
      "#m rect{/*x*/fill:red;}",
      "#m rect{fill:red;}}orphan{",
    ]) {
      expect(
        new DOMParser().parseFromString(scoped(hostile), "image/svg+xml").querySelector("style"),
      ).toBeNull();
    }
    // Mermaid's ever-present @keyframes strip away without costing the sheet.
    const keyframed = new DOMParser().parseFromString(
      scoped("@keyframes dash{to{stroke-dashoffset:0;}}#m rect{fill:red;}"),
      "image/svg+xml",
    );
    expect(keyframed.querySelector("style")?.textContent).toContain("rect { fill: red; }");
    expect(keyframed.querySelector("style")?.textContent).not.toContain("keyframes");
    // A root without a safe id keeps no stylesheet at all.
    const unidentified = sanitizeSvg(
      '<svg xmlns="http://www.w3.org/2000/svg"><style>svg{fill:red;}</style><rect /></svg>',
      "request-flow",
    );
    expect(
      new DOMParser().parseFromString(unidentified, "image/svg+xml").querySelector("style"),
    ).toBeNull();
  });

  test("rejects network-capable sources before a renderer can start a request", async () => {
    const fetch = vi.fn(() => Promise.reject(new Error("Network access is not allowed.")));
    vi.stubGlobal("fetch", fetch);

    await expect(
      renderSpecBlock(
        "mermaid",
        'flowchart LR\n  A@{ img: "https://example.com/a.png" }',
        "unsafe-mermaid",
      ),
    ).rejects.toThrow("Mermaid source");
    await expect(
      renderSpecBlock("d2", "A.icon: https://example.com/a.png", "unsafe-d2"),
    ).rejects.toThrow("D2 source");
    await expect(
      renderSpecBlock(
        "flint",
        JSON.stringify({
          data: { values: [{ category: "A", value: 1 }], url: "https://example.com/a.csv" },
          chart_spec: {
            chartType: "Bar Chart",
            encodings: { x: { field: "category" }, y: { field: "value" } },
          },
        }),
        "unsafe-flint",
      ),
    ).rejects.toThrow("Flint field");
    expect(fetch).not.toHaveBeenCalled();
  });

  test("opens an inline mini-chat pinned to the selected block id", async () => {
    const user = userEvent.setup();
    const onIterate = vi.fn(async () => {});
    render(
      <SpecBlockView
        attrs={attrs({ cachedRender: cached("flowchart LR\n  A --> B") })}
        sectionId="design"
        onIterate={onIterate}
      />,
    );

    await user.click(screen.getByRole("img", { name: "Mermaid diagram" }));
    expect(screen.getByText("Pinned to block")).toBeTruthy();
    await user.type(screen.getByLabelText("Message about block request-flow"), "Add a retry path.");
    await user.click(screen.getByRole("button", { name: "Send message about block request-flow" }));

    await waitFor(() =>
      expect(onIterate).toHaveBeenCalledWith({
        sectionId: "design",
        blockId: "request-flow",
        message: "Add a retry path.",
      }),
    );
    expect(
      screen.getByText("The agent will update this block source with spec_update_block."),
    ).toBeTruthy();
  });

  test("regenerates the visible render after the block source changes", async () => {
    const onCache = vi.fn();
    vi.spyOn(specBlockRenderAdapters.mermaid, "render").mockResolvedValue(
      '<svg xmlns="http://www.w3.org/2000/svg"><text>New render</text></svg>',
    );
    const { rerender } = render(
      <SpecBlockView
        attrs={attrs({
          source: "old source",
          cachedRender: {
            kind: "mermaid",
            source: "old source",
            blockId: "request-flow",
            rendererRevision: SPEC_BLOCK_RENDERER_REVISION,
            svg: '<svg xmlns="http://www.w3.org/2000/svg"><text>Old render</text></svg>',
          },
        })}
        onCache={onCache}
      />,
    );
    expect(screen.getByText("Old render")).toBeTruthy();

    rerender(
      <SpecBlockView
        attrs={attrs({
          source: "new source",
          cachedRender: null,
        })}
        onCache={onCache}
      />,
    );

    expect(await screen.findByText("New render")).toBeTruthy();
    expect(screen.queryByText("Old render")).toBeNull();
    await waitFor(() =>
      expect(onCache).toHaveBeenCalledWith(
        expect.objectContaining({ kind: "mermaid", source: "new source" }),
      ),
    );
  });
});

function attrs(overrides: Partial<SpecBlockAttrs> = {}): SpecBlockAttrs {
  return {
    id: "request-flow",
    kind: "mermaid",
    source: "flowchart LR\n  A --> B",
    cachedRender: null,
    provenance: { type: "illustrative" },
    ...overrides,
  };
}

function cached(source: string) {
  return {
    kind: "mermaid",
    source,
    blockId: "request-flow",
    rendererRevision: SPEC_BLOCK_RENDERER_REVISION,
    svg: '<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0" /></svg>',
  };
}
