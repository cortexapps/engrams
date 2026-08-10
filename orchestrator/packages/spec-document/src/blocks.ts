import type { Node as ProseMirrorNode } from "prosemirror-model";

export const SPEC_BLOCK_KINDS = ["mermaid", "d2", "flint"] as const;
export const SPEC_RENDER_TARGETS = ["engrams", "github"] as const;

/** Change this value when product renderer output can change. */
export const SPEC_BLOCK_RENDERER_REVISION = "2";
export const SPEC_BLOCK_CACHE_MAX_BYTES = 512 * 1024;

export type SpecBlockKind = (typeof SPEC_BLOCK_KINDS)[number];
export type SpecRenderTarget = (typeof SPEC_RENDER_TARGETS)[number];
export type SpecBlockExportMode = "source" | "cached-render";

export interface SpecBlockRegistration {
  kind: SpecBlockKind;
  label: string;
}

export interface SpecRenderTargetRegistration {
  target: SpecRenderTarget;
  blockModes: Readonly<Record<SpecBlockKind, SpecBlockExportMode>>;
}

export type SpecBlockProvenance =
  | { type: "verified"; caption: string }
  | { type: "illustrative"; caption?: string };

export interface SpecBlockCachedRender {
  kind: string;
  source: string;
  blockId: string;
  rendererRevision: string;
  svg: string;
}

export interface SpecBlockAttrs {
  id: string;
  kind: string;
  source: string;
  cachedRender: SpecBlockCachedRender | null;
  provenance: SpecBlockProvenance;
}

export function encodedSpecBlockCacheSize(cache: unknown): number {
  const encoded = JSON.stringify(cache);
  if (encoded === undefined) {
    throw new Error("A cached render must be a JSON object");
  }
  return new TextEncoder().encode(encoded).byteLength;
}

export function validateSpecBlockCachedRender(value: unknown): SpecBlockCachedRender {
  if (!isRecord(value)) {
    throw new Error("A cached render must be an object");
  }
  const expectedKeys = ["blockId", "kind", "rendererRevision", "source", "svg"];
  const keys = Object.keys(value).sort();
  if (
    keys.length !== expectedKeys.length ||
    keys.some((key, index) => key !== expectedKeys[index])
  ) {
    throw new Error(
      "A cached render must contain only blockId, kind, rendererRevision, source, and svg",
    );
  }
  if (
    typeof value.kind !== "string" ||
    typeof value.source !== "string" ||
    typeof value.blockId !== "string" ||
    typeof value.rendererRevision !== "string" ||
    typeof value.svg !== "string"
  ) {
    throw new Error("Every cached render field must be a string");
  }
  return {
    kind: value.kind,
    source: value.source,
    blockId: value.blockId,
    rendererRevision: value.rendererRevision,
    svg: value.svg,
  };
}

const registrations: Readonly<Record<SpecBlockKind, SpecBlockRegistration>> = {
  mermaid: { kind: "mermaid", label: "Mermaid diagram" },
  d2: { kind: "d2", label: "D2 diagram" },
  flint: { kind: "flint", label: "Flint chart" },
};

const renderTargetRegistrations: Readonly<Record<SpecRenderTarget, SpecRenderTargetRegistration>> =
  {
    engrams: {
      target: "engrams",
      blockModes: { mermaid: "source", d2: "source", flint: "source" },
    },
    github: {
      target: "github",
      blockModes: { mermaid: "source", d2: "cached-render", flint: "cached-render" },
    },
  };

export const specBlockRegistry: readonly SpecBlockRegistration[] = SPEC_BLOCK_KINDS.map(
  (kind) => registrations[kind],
);

export function specBlockRegistration(kind: string): SpecBlockRegistration | null {
  return isSpecBlockKind(kind) ? registrations[kind] : null;
}

export function specRenderTargetRegistration(
  target: SpecRenderTarget,
): SpecRenderTargetRegistration {
  return renderTargetRegistrations[target];
}

export function isSpecBlockKind(kind: string): kind is SpecBlockKind {
  return SPEC_BLOCK_KINDS.some((candidate) => candidate === kind);
}

export function specNodesSemanticallyEqual(left: ProseMirrorNode, right: ProseMirrorNode): boolean {
  return JSON.stringify(semanticSpecNodeJson(left)) === JSON.stringify(semanticSpecNodeJson(right));
}

export function semanticSpecNodeJson(node: ProseMirrorNode): unknown {
  return omitRenderCache(node.toJSON());
}

export function readSpecBlockAttrs(attrs: Readonly<Record<string, unknown>>): SpecBlockAttrs {
  const id = typeof attrs.id === "string" && attrs.id.length > 0 ? attrs.id : "unknown";
  const kind = typeof attrs.kind === "string" && attrs.kind.length > 0 ? attrs.kind : "unknown";
  const source = typeof attrs.source === "string" ? attrs.source : "";
  return {
    id,
    kind,
    source,
    cachedRender: readCachedRender(attrs.cachedRender),
    provenance: readProvenance(attrs.provenance),
  };
}

export function matchingSpecBlockCachedRender(attrs: SpecBlockAttrs): SpecBlockCachedRender | null {
  const cache = attrs.cachedRender;
  if (
    cache === null ||
    cache.kind !== attrs.kind ||
    cache.source !== attrs.source ||
    cache.blockId !== attrs.id ||
    cache.rendererRevision !== SPEC_BLOCK_RENDERER_REVISION
  ) {
    return null;
  }
  return cache;
}

function readCachedRender(value: unknown): SpecBlockCachedRender | null {
  try {
    return validateSpecBlockCachedRender(value);
  } catch {
    return null;
  }
}

function readProvenance(value: unknown): SpecBlockProvenance {
  if (!isRecord(value) || value.type !== "verified") {
    const caption =
      isRecord(value) && typeof value.caption === "string" ? value.caption : undefined;
    return caption ? { type: "illustrative", caption } : { type: "illustrative" };
  }
  return {
    type: "verified",
    caption:
      typeof value.caption === "string" && value.caption.trim().length > 0
        ? value.caption
        : "Verified from repository sources.",
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function omitRenderCache(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(omitRenderCache);
  if (!isRecord(value)) return value;

  const result: Record<string, unknown> = {};
  for (const [key, child] of Object.entries(value)) {
    if (value.type === "diagramBlock" && key === "attrs" && isRecord(child)) {
      const { cachedRender: _cachedRender, ...semanticAttrs } = child;
      result[key] = omitRenderCache(semanticAttrs);
    } else {
      result[key] = omitRenderCache(child);
    }
  }
  return result;
}
