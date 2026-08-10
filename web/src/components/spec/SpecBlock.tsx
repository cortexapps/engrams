import { useEffect, useMemo, useState } from "react";
import {
  isSpecBlockKind,
  readSpecBlockAttrs,
  specBlockRegistration,
  type SpecBlockAttrs,
  type SpecBlockCachedRender,
} from "@engrams/spec-document";
import { NodeViewWrapper, type NodeViewProps } from "@tiptap/react";

import { renderSpecBlock, sanitizeSvg } from "./block-renderers";
import "./spec-block.css";

interface RenderedBlock {
  key: string;
  svg: string;
}

interface SpecBlockViewProps {
  attrs: SpecBlockAttrs;
  onCache?: (render: SpecBlockCachedRender) => void;
}

export function SpecBlock({ node, editor, updateAttributes }: NodeViewProps) {
  const attrs = readSpecBlockAttrs(node.attrs);
  return (
    <SpecBlockView
      attrs={attrs}
      onCache={
        editor.isEditable
          ? (cachedRender) => {
              updateAttributes({ cachedRender });
            }
          : undefined
      }
    />
  );
}

export function SpecBlockView({ attrs, onCache }: SpecBlockViewProps) {
  const { id, kind, source, provenance } = attrs;
  const registration = specBlockRegistration(kind);
  const renderKey = `${id}\u0000${kind}\u0000${source}`;
  const cachedSvg = useMemo(() => readCachedSvg(attrs), [attrs.cachedRender, source]);
  const [rendered, setRendered] = useState<RenderedBlock | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (cachedSvg || !registration || !isSpecBlockKind(kind)) return;
    let current = true;
    setError(null);
    void renderSpecBlock(kind, source, id)
      .then((svg) => {
        if (!current) return;
        setRendered({ key: renderKey, svg });
        onCache?.({ kind, source, svg });
      })
      .catch((cause: unknown) => {
        if (!current) return;
        setError(cause instanceof Error ? cause.message : "The block renderer failed.");
      });
    return () => {
      current = false;
    };
  }, [cachedSvg, id, kind, onCache, registration, renderKey, source]);

  const svg = cachedSvg ?? (rendered?.key === renderKey ? rendered.svg : null);
  const label = registration?.label ?? `Unknown block: ${kind}`;
  const provenanceLabel = provenance.type === "verified" ? "Verified" : "Illustrative";

  return (
    <NodeViewWrapper
      as="figure"
      className="spec-block"
      data-block-id={id}
      data-block-kind={kind}
      data-provenance={provenance.type}
      contentEditable={false}
    >
      <figcaption className="spec-block-heading">
        <span className="spec-block-kind">{label}</span>
        <span className="spec-block-provenance">{provenanceLabel}</span>
      </figcaption>
      {provenance.caption ? <p className="spec-block-caption">{provenance.caption}</p> : null}
      {svg ? (
        <div
          className="spec-block-render"
          role="img"
          aria-label={label}
          dangerouslySetInnerHTML={{ __html: svg }}
        />
      ) : !registration ? (
        <SourceFallback label={`Unsupported block kind: ${kind}`} source={source} />
      ) : error ? (
        <SourceFallback label={`${label} could not render: ${error}`} source={source} />
      ) : (
        <div className="spec-block-loading" role="status">
          Rendering {label.toLowerCase()}…
        </div>
      )}
    </NodeViewWrapper>
  );
}

function SourceFallback({ label, source }: { label: string; source: string }) {
  return (
    <div className="spec-block-fallback">
      <div className="spec-block-fallback-label">{label}</div>
      <pre aria-label={label}>
        <code>{source}</code>
      </pre>
    </div>
  );
}

function readCachedSvg(attrs: SpecBlockAttrs): string | null {
  if (
    !attrs.cachedRender ||
    attrs.cachedRender.kind !== attrs.kind ||
    attrs.cachedRender.source !== attrs.source
  ) {
    return null;
  }
  try {
    return sanitizeSvg(attrs.cachedRender.svg);
  } catch {
    return null;
  }
}
