import { afterEach, describe, expect, test, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import type { SpecBlockAttrs } from "@engrams/spec-document";

import { SpecBlockView } from "./SpecBlock";
import { sanitizeSvg, specBlockRenderAdapters } from "./block-renderers";

const canvasContextDescriptor = Object.getOwnPropertyDescriptor(
  HTMLCanvasElement.prototype,
  "getContext",
);

afterEach(() => {
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

    const diagram = await screen.findByRole("img", { name: "Mermaid diagram" });
    expect(diagram.querySelector("svg")).not.toBeNull();
    expect(fetch).not.toHaveBeenCalled();
    await waitFor(() => expect(onCache).toHaveBeenCalledOnce());
    expect(onCache.mock.calls[0]?.[0]).toMatchObject({ source: "flowchart LR\n  A --> B" });
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

  test("registers each product renderer and removes active or external SVG content", () => {
    expect(Object.keys(specBlockRenderAdapters)).toEqual(["mermaid", "d2", "flint"]);
    const sanitized = sanitizeSvg(
      '<svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)">' +
        '<script>alert(1)</script><image href="https://example.com/a.png" />' +
        '<rect style="fill: url(https://example.com/a.svg)" /></svg>',
    );
    const svg = new DOMParser().parseFromString(sanitized, "image/svg+xml");
    expect(svg.querySelector("script")).toBeNull();
    expect(svg.querySelector("image")?.hasAttribute("href")).toBe(false);
    expect(svg.querySelector("rect")?.hasAttribute("style")).toBe(false);
    expect(svg.documentElement.hasAttribute("onload")).toBe(false);
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
    svg: '<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0" /></svg>',
  };
}
