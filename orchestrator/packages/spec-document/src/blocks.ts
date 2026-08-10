import type { Node as ProseMirrorNode } from "prosemirror-model";

export const SPEC_BLOCK_KINDS = ["mermaid", "d2", "flint"] as const;

export type SpecBlockKind = (typeof SPEC_BLOCK_KINDS)[number];

export interface SpecBlockRegistration {
  kind: SpecBlockKind;
  label: string;
}

export type SpecBlockProvenance =
  | { type: "verified"; caption: string }
  | { type: "illustrative"; caption?: string };

export interface SpecBlockCachedRender {
  kind: string;
  source: string;
  svg: string;
}

export interface SpecBlockAttrs {
  id: string;
  kind: string;
  source: string;
  cachedRender: SpecBlockCachedRender | null;
  provenance: SpecBlockProvenance;
}

const registrations: Readonly<Record<SpecBlockKind, SpecBlockRegistration>> = {
  mermaid: { kind: "mermaid", label: "Mermaid diagram" },
  d2: { kind: "d2", label: "D2 diagram" },
  flint: { kind: "flint", label: "Flint chart" },
};

export const specBlockRegistry: readonly SpecBlockRegistration[] = SPEC_BLOCK_KINDS.map(
  (kind) => registrations[kind],
);

export function specBlockRegistration(kind: string): SpecBlockRegistration | null {
  return isSpecBlockKind(kind) ? registrations[kind] : null;
}

export function isSpecBlockKind(kind: string): kind is SpecBlockKind {
  return SPEC_BLOCK_KINDS.some((candidate) => candidate === kind);
}

export function specNodesSemanticallyEqual(
  left: ProseMirrorNode,
  right: ProseMirrorNode,
): boolean {
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

function readCachedRender(value: unknown): SpecBlockCachedRender | null {
  if (
    !isRecord(value) ||
    typeof value.kind !== "string" ||
    typeof value.source !== "string" ||
    typeof value.svg !== "string"
  ) {
    return null;
  }
  return { kind: value.kind, source: value.source, svg: value.svg };
}

function readProvenance(value: unknown): SpecBlockProvenance {
  if (!isRecord(value) || value.type !== "verified") {
    const caption = isRecord(value) && typeof value.caption === "string" ? value.caption : undefined;
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
