import {
  useEffect,
  useMemo,
  useState,
  type FormEvent,
  type MouseEvent as ReactMouseEvent,
} from "react";
import {
  encodedSpecBlockCacheSize,
  isSpecBlockKind,
  matchingSpecBlockCachedRender,
  readSpecBlockAttrs,
  SPEC_BLOCK_CACHE_MAX_BYTES,
  SPEC_BLOCK_RENDERER_REVISION,
  specBlockRegistration,
  type SpecBlockAttrs,
  type SpecBlockCachedRender,
} from "@engrams/spec-document";
import { NodeViewWrapper, type NodeViewProps } from "@tiptap/react";
import { Loader2Icon, MessageSquareIcon, SendIcon } from "lucide-react";

import { renderSpecBlock, sanitizeSvg } from "./block-renderers";
import { useSpecBlockIteration, type SpecBlockIterationRequest } from "./block-iteration";
import "./spec-block.css";

interface RenderedBlock {
  key: string;
  svg: string;
}

interface SpecBlockViewProps {
  attrs: SpecBlockAttrs;
  onCache?: (render: SpecBlockCachedRender) => void;
  sectionId?: string;
  onIterate?: (request: SpecBlockIterationRequest) => Promise<void>;
}

export function SpecBlock({ node, editor, updateAttributes, getPos }: NodeViewProps) {
  const attrs = readSpecBlockAttrs(node.attrs);
  const onIterate = useSpecBlockIteration();
  return (
    <SpecBlockView
      attrs={attrs}
      sectionId={sectionIdAt(editor.state.doc, getPos())}
      onIterate={onIterate ?? undefined}
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

export function SpecBlockView({ attrs, onCache, sectionId, onIterate }: SpecBlockViewProps) {
  const { id, kind, source, provenance } = attrs;
  const registration = specBlockRegistration(kind);
  const renderKey = `${SPEC_BLOCK_RENDERER_REVISION}\u0000${id}\u0000${kind}\u0000${source}`;
  const cachedSvg = useMemo(() => readCachedSvg(attrs), [attrs.cachedRender, id, kind, source]);
  const [rendered, setRendered] = useState<RenderedBlock | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [chatOpen, setChatOpen] = useState(false);
  const [message, setMessage] = useState("");
  const [sentMessage, setSentMessage] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const [sendError, setSendError] = useState<string | null>(null);

  useEffect(() => {
    if (cachedSvg || !registration || !isSpecBlockKind(kind)) return;
    let current = true;
    setError(null);
    void renderSpecBlock(kind, source, id)
      .then((svg) => {
        if (!current) return;
        setRendered({ key: renderKey, svg });
        const cachedRender = {
          kind,
          source,
          blockId: id,
          rendererRevision: SPEC_BLOCK_RENDERER_REVISION,
          svg,
        };
        if (encodedSpecBlockCacheSize(cachedRender) <= SPEC_BLOCK_CACHE_MAX_BYTES) {
          onCache?.(cachedRender);
        }
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
  const canIterate = sectionId !== undefined && onIterate !== undefined;

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    const text = message.trim();
    if (!canIterate || !text || sending) return;
    setSending(true);
    setSendError(null);
    try {
      await onIterate({ sectionId, blockId: id, message: text });
      setSentMessage(text);
      setMessage("");
    } catch (cause: unknown) {
      setSendError(cause instanceof Error ? cause.message : "The block request failed.");
    } finally {
      setSending(false);
    }
  };

  return (
    <NodeViewWrapper
      as="figure"
      className="spec-block"
      data-block-id={id}
      data-block-kind={kind}
      data-provenance={provenance.type}
      contentEditable={false}
      onClick={(event: ReactMouseEvent<HTMLElement>) => {
        if (!canIterate || chatOpen) return;
        if ((event.target as Element).closest("button, textarea, input, a")) return;
        setChatOpen(true);
      }}
    >
      <figcaption className="spec-block-heading">
        <span className="spec-block-kind">{label}</span>
        <span className="spec-block-heading-actions">
          <span className="spec-block-provenance">{provenanceLabel}</span>
          {canIterate ? (
            <button
              type="button"
              className="spec-block-iterate-trigger"
              aria-expanded={chatOpen}
              onClick={() => setChatOpen((open) => !open)}
            >
              <MessageSquareIcon aria-hidden="true" />
              Iterate
            </button>
          ) : null}
        </span>
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
      {chatOpen && canIterate ? (
        <div className="spec-block-chat" onClick={(event) => event.stopPropagation()}>
          <div className="spec-block-chat-title">
            <MessageSquareIcon aria-hidden="true" />
            <span>Iterate on {id}</span>
            <span className="spec-block-chat-pin">Pinned to block</span>
          </div>
          {sentMessage ? (
            <div className="spec-block-chat-receipt" role="status">
              <span>{sentMessage}</span>
              <small>The agent will update this block source with spec_update_block.</small>
            </div>
          ) : null}
          <form className="spec-block-chat-composer" onSubmit={submit}>
            <textarea
              aria-label={`Message about block ${id}`}
              value={message}
              maxLength={20_000}
              placeholder="Ask for a change to this block…"
              onChange={(event) => setMessage(event.target.value)}
            />
            <button
              type="submit"
              aria-label={`Send message about block ${id}`}
              disabled={sending || message.trim().length === 0}
            >
              {sending ? <Loader2Icon className="spec-block-spin" /> : <SendIcon />}
            </button>
          </form>
          {sendError ? <p className="spec-block-chat-error">{sendError}</p> : null}
        </div>
      ) : null}
    </NodeViewWrapper>
  );
}

function sectionIdAt(
  document: NodeViewProps["editor"]["state"]["doc"],
  position: number | undefined,
) {
  if (position === undefined) return undefined;
  const resolved = document.resolve(position);
  for (let depth = resolved.depth; depth >= 0; depth -= 1) {
    const node = resolved.node(depth);
    if (node.type.name === "section" && typeof node.attrs.id === "string") {
      return node.attrs.id;
    }
  }
  return undefined;
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
  const cache = matchingSpecBlockCachedRender(attrs);
  if (!cache) return null;
  try {
    return sanitizeSvg(cache.svg, attrs.id);
  } catch {
    return null;
  }
}
