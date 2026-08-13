import type { SpecBlockKind } from "@engrams/spec-document";
import type { ChartAssemblyInput } from "flint-chart";
import type { VisualizationSpec } from "vega-embed";

import {
  assertNoNetworkValues,
  assertSafeD2Source,
  assertSafeMermaidSource,
  sanitizeSvg,
} from "./svg-safety";
import { D2Client } from "./d2-client";

export { sanitizeSvg } from "./svg-safety";

export interface SpecBlockRenderAdapter {
  kind: SpecBlockKind;
  render(source: string, blockId: string): Promise<string>;
}

let mermaidInitialized = false;
let d2: D2Client | null = null;

const mermaidAdapter: SpecBlockRenderAdapter = {
  kind: "mermaid",
  async render(source, blockId) {
    assertSafeMermaidSource(source);
    const { default: mermaid } = await import("mermaid");
    if (!mermaidInitialized) {
      mermaid.initialize({
        startOnLoad: false,
        securityLevel: "strict",
        // Both levels: current Mermaid reads the TOP-LEVEL flag for node
        // labels — with only the flowchart-scoped one it still emits
        // foreignObject HTML labels, which sanitizeSvg strips, and every
        // node renders empty. SVG <text> labels survive the sanitizer.
        htmlLabels: false,
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
    assertSafeD2Source(source);
    const client = (d2 ??= new D2Client());
    try {
      const result = await client.compile({
        fs: { "index.d2": source },
        inputPath: "index.d2",
        options: {
          layout: "dagre",
          noXMLTag: true,
          salt: blockId,
        },
      });
      return await client.render(result.diagram, {
        ...result.renderOptions,
        noXMLTag: true,
        salt: blockId,
      });
    } catch (error) {
      if (client.failed && d2 === client) d2 = null;
      throw error;
    }
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
    assertNoNetworkValues(specification, "assembled Flint specification");
    const host = document.createElement("div");
    const result = await vegaEmbed(host, specification, {
      actions: false,
      ast: true,
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
  return sanitizeSvg(await specBlockRenderAdapters[kind].render(source, blockId), blockId);
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
  assertNoNetworkValues(value, "Flint source");
  return value;
}

function renderId(kind: SpecBlockKind, blockId: string): string {
  const safeId = blockId.replace(/[^a-zA-Z0-9_-]/g, "-");
  return `spec-${kind}-${safeId}`;
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
