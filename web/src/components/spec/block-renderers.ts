import type { D2 } from "@terrastruct/d2";
import type { SpecBlockKind } from "@engrams/spec-document";
import type { ChartAssemblyInput } from "flint-chart";
import type { VisualizationSpec } from "vega-embed";

export interface SpecBlockRenderAdapter {
  kind: SpecBlockKind;
  render(source: string, blockId: string): Promise<string>;
}

let mermaidInitialized = false;
let d2: D2 | null = null;

const mermaidAdapter: SpecBlockRenderAdapter = {
  kind: "mermaid",
  async render(source, blockId) {
    const { default: mermaid } = await import("mermaid");
    if (!mermaidInitialized) {
      mermaid.initialize({
        startOnLoad: false,
        securityLevel: "strict",
        flowchart: { htmlLabels: false },
      });
      mermaidInitialized = true;
    }
    const result = await mermaid.render(renderId("mermaid", blockId), source);
    return result.svg;
  },
};

const d2Adapter: SpecBlockRenderAdapter = {
  kind: "d2",
  async render(source, blockId) {
    const { D2: D2Renderer } = await import("@terrastruct/d2");
    d2 ??= new D2Renderer();
    const result = await d2.compile({
      fs: { "index.d2": source },
      inputPath: "index.d2",
      options: {
        layout: "dagre",
        noXMLTag: true,
        salt: blockId,
      },
    });
    return d2.render(result.diagram, {
      ...result.renderOptions,
      noXMLTag: true,
      salt: blockId,
    });
  },
};

const flintAdapter: SpecBlockRenderAdapter = {
  kind: "flint",
  async render(source) {
    const input = parseFlintSource(source);
    const [{ assembleVegaLite }, { default: vegaEmbed }] = await Promise.all([
      import("flint-chart/vegalite"),
      import("vega-embed"),
    ]);
    const specification: VisualizationSpec = assembleVegaLite(input);
    const host = document.createElement("div");
    const result = await vegaEmbed(host, specification, {
      actions: false,
      renderer: "svg",
    });
    try {
      return await result.view.toSVG();
    } finally {
      result.finalize();
    }
  },
};

export const specBlockRenderAdapters: Readonly<Record<SpecBlockKind, SpecBlockRenderAdapter>> = {
  mermaid: mermaidAdapter,
  d2: d2Adapter,
  flint: flintAdapter,
};

export async function renderSpecBlock(kind: SpecBlockKind, source: string, blockId: string) {
  return sanitizeSvg(await specBlockRenderAdapters[kind].render(source, blockId));
}

export function sanitizeSvg(svgSource: string): string {
  const parsed = new DOMParser().parseFromString(svgSource, "image/svg+xml");
  if (parsed.querySelector("parsererror") || parsed.documentElement.localName !== "svg") {
    throw new Error("The block renderer returned invalid SVG.");
  }

  for (const element of [
    ...parsed.querySelectorAll("script, foreignObject, iframe, object, embed"),
  ]) {
    element.remove();
  }
  for (const element of [parsed.documentElement, ...parsed.querySelectorAll("*")]) {
    for (const attribute of [...element.attributes]) {
      const name = attribute.name.toLowerCase();
      if (name.startsWith("on")) {
        element.removeAttribute(attribute.name);
      } else if (name === "href" || name === "xlink:href" || name === "src") {
        if (!isSelfContainedReference(attribute.value)) element.removeAttribute(attribute.name);
      } else if (name === "style" && hasExternalCss(attribute.value)) {
        element.removeAttribute(attribute.name);
      }
    }
    if (element.localName === "style" && hasExternalCss(element.textContent ?? "")) {
      element.remove();
    }
  }

  parsed.documentElement.setAttribute("role", "presentation");
  parsed.documentElement.removeAttribute("width");
  parsed.documentElement.removeAttribute("height");
  return new XMLSerializer().serializeToString(parsed.documentElement);
}

function parseFlintSource(source: string): ChartAssemblyInput {
  let value: unknown;
  try {
    value = JSON.parse(source);
  } catch {
    throw new Error("A Flint source specification must be valid JSON.");
  }
  if (!isChartAssemblyInput(value)) {
    throw new Error("A Flint block must contain inline data.values and chart_spec.chartType.");
  }
  return value;
}

function renderId(kind: SpecBlockKind, blockId: string): string {
  const safeId = blockId.replace(/[^a-zA-Z0-9_-]/g, "-");
  return `spec-${kind}-${safeId}`;
}

function isSelfContainedReference(value: string): boolean {
  const reference = value.trim();
  return reference.startsWith("#") || /^data:image\/(?:png|jpeg|gif|webp);base64,/i.test(reference);
}

function hasExternalCss(value: string): boolean {
  return /@import|(?:https?:)?\/\/|javascript:/i.test(value);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isChartAssemblyInput(value: unknown): value is ChartAssemblyInput {
  return (
    isRecord(value) &&
    isRecord(value.data) &&
    Array.isArray(value.data.values) &&
    isRecord(value.chart_spec) &&
    typeof value.chart_spec.chartType === "string"
  );
}
