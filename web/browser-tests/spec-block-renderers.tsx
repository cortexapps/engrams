import React from "react";
import { flushSync } from "react-dom";
import { createRoot } from "react-dom/client";
import type { SpecBlockAttrs, SpecBlockKind } from "@engrams/spec-document";

import { SpecBlockView } from "../src/components/spec/SpecBlock";
import { renderSpecBlock, sanitizeSvg } from "../src/components/spec/block-renderers";

declare global {
  interface Window {
    runSpecBlockRendererTests: () => Promise<BrowserTestResult>;
  }
}

interface BrowserTestResult {
  rendered: Record<SpecBlockKind, boolean>;
  deterministic: Record<SpecBlockKind, boolean>;
  unsafeRejected: boolean;
  svgVectorsRejected: boolean;
  fallbackRendered: boolean;
}

const sources: Record<SpecBlockKind, string> = {
  mermaid: "flowchart LR\n  Browser --> Renderer",
  d2: "direction: right\nBrowser -> Renderer",
  flint: JSON.stringify({
    data: {
      values: [
        { category: "A", value: 1 },
        { category: "B", value: 2 },
      ],
    },
    semantic_types: { category: "Category", value: "Quantity" },
    chart_spec: {
      chartType: "Bar Chart",
      encodings: { x: { field: "category" }, y: { field: "value" } },
      baseSize: { width: 320, height: 220 },
    },
  }),
};

window.runSpecBlockRendererTests = async () => {
  const rendered = {} as Record<SpecBlockKind, boolean>;
  const deterministic = {} as Record<SpecBlockKind, boolean>;
  for (const kind of ["mermaid", "d2", "flint"] as const) {
    const first = await renderSpecBlock(kind, sources[kind], `browser-${kind}`);
    const second = await renderSpecBlock(kind, sources[kind], `browser-${kind}`);
    rendered[kind] = first.startsWith("<svg") && first.includes('role="presentation"');
    deterministic[kind] = second === first;
  }

  const unsafeSources: Array<[SpecBlockKind, string]> = [
    ["mermaid", 'flowchart LR\n  A@{ img: "https://example.invalid/a.png" }'],
    ["d2", "A.icon: https://example.invalid/a.png"],
    [
      "flint",
      JSON.stringify({
        data: { values: [{ value: 1 }], url: "https://example.invalid/a.csv" },
        chart_spec: { chartType: "Bar Chart", encodings: { y: { field: "value" } } },
      }),
    ],
  ];
  const unsafeRejected = (
    await Promise.all(
      unsafeSources.map(async ([kind, source]) => {
        try {
          await renderSpecBlock(kind, source, `unsafe-${kind}`);
          return false;
        } catch {
          return true;
        }
      }),
    )
  ).every(Boolean);

  const sanitized = sanitizeSvg(
    '<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" onload="alert(1)">' +
      "<script>alert(1)</script><foreignObject><div>foreign</div></foreignObject>" +
      '<animate attributeName="href" to="https://example.invalid/a" />' +
      '<set attributeName="fill" to="url(https://example.invalid/a.svg)" />' +
      "<style>@import url(https://example.invalid/a.css); .x{fill:red}</style>" +
      '<a href="https://example.invalid"><text>Unlinked</text></a>' +
      '<image href="data:image/png;base64,AAAA" />' +
      '<rect fill="url(https://example.invalid/a.svg)" style="fill:u\\72l(https://example.invalid/a.svg)" />' +
      '<path id="safe-path" d="M0 0" marker-end="url(#safe-marker)" />' +
      '<use href="#safe-path" xlink:href="https://example.invalid/b.svg#x" />' +
      "</svg>",
    "browser-safety",
  );
  const sanitizedDocument = new DOMParser().parseFromString(sanitized, "image/svg+xml");
  const safePathId = sanitizedDocument.querySelector("path")?.id;
  const svgVectorsRejected =
    sanitizedDocument.querySelector("script, foreignObject, animate, set, style, a, image") ===
      null &&
    sanitizedDocument.querySelector("rect")?.attributes.length === 0 &&
    safePathId?.startsWith("spec-block-") === true &&
    sanitizedDocument
      .querySelector("path")
      ?.getAttribute("marker-end")
      ?.startsWith("url(#spec-block-") === true &&
    sanitizedDocument.querySelector("use")?.getAttribute("href") === `#${safePathId}` &&
    sanitizedDocument.querySelector("use")?.hasAttribute("xlink:href") === false &&
    sanitizeSvg(sanitized, "browser-safety") === sanitized;

  const rootElement = document.getElementById("root");
  if (!rootElement) throw new Error("The browser test root is missing.");
  const root = createRoot(rootElement);
  const fallback: SpecBlockAttrs = {
    id: "unknown-block",
    kind: "future-renderer",
    source: "raw source remains visible",
    cachedRender: null,
    provenance: { type: "illustrative" },
  };
  flushSync(() => root.render(<SpecBlockView attrs={fallback} />));
  const fallbackRendered =
    rootElement.textContent?.includes("Unsupported block kind: future-renderer") === true &&
    rootElement.textContent.includes("raw source remains visible");
  root.unmount();

  return { rendered, deterministic, unsafeRejected, svgVectorsRejected, fallbackRendered };
};
