import { createContext, useContext, useState, type ReactNode } from "react";
import { NodeViewContent, NodeViewWrapper, type NodeViewProps } from "@tiptap/react";
import type { SectionState } from "@engrams/spec-document";

import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Text } from "@/components/ui/text";
import { Textarea } from "@/components/ui/textarea";
import { SectionGlyph } from "./SectionGlyph";
import type { SpecSurface } from "./spec-surface";

export interface SectionStateAction {
  sectionId: string;
  state: SectionState;
  reason?: string;
}

export interface SectionNodeViewContextValue {
  surface: SpecSurface;
  showProvenance: boolean;
  pendingSectionId: string | null;
  setSectionState: (action: SectionStateAction) => void;
}

const SectionNodeViewContext = createContext<SectionNodeViewContextValue | null>(null);

export function useSectionNodeViewContext(): SectionNodeViewContextValue | null {
  return useContext(SectionNodeViewContext);
}

export function SectionNodeViewProvider({
  value,
  children,
}: {
  value: SectionNodeViewContextValue;
  children: ReactNode;
}) {
  return (
    <SectionNodeViewContext.Provider value={value}>{children}</SectionNodeViewContext.Provider>
  );
}

export function SectionNodeView({ editor, getPos, node }: NodeViewProps) {
  const context = useSectionNodeViewContext();
  const sectionId = typeof node.attrs.id === "string" ? node.attrs.id : null;
  const section = context?.surface.sections.find((candidate) => candidate.id === sectionId);

  if (!context || !section || sectionId === null) {
    return (
      <NodeViewWrapper as="section" data-section-id={sectionId ?? undefined}>
        <NodeViewContent />
      </NodeViewWrapper>
    );
  }

  const pending = context.pendingSectionId === sectionId;
  const focusBody = () => {
    const sectionPosition = typeof getPos === "function" ? getPos() : undefined;
    if (typeof sectionPosition !== "number") return;
    const heading = node.firstChild;
    const bodyPosition = sectionPosition + 1 + (heading?.nodeSize ?? 0) + 1;
    editor.commands.focus(bodyPosition, { scrollIntoView: false });
  };
  const setState = (state: SectionState, reason?: string) =>
    context.setSectionState({ sectionId, state, ...(reason ? { reason } : {}) });

  return (
    <NodeViewWrapper
      as="section"
      className={section.state === "proposed" ? "spec-mode-proposal" : undefined}
      data-proposal-treatment={section.state === "proposed" ? "gutter" : undefined}
      data-section-id={sectionId}
      data-state={section.state}
      data-reached={section.isReached ? "true" : "false"}
    >
      <header className="spec-mode-section-heading" contentEditable={false}>
        <SectionGlyph section={section} />
        <Text as="h2" variant="heading">
          {section.title}
        </Text>
        {section.state === "settled" && section.credit ? (
          <Text as="span" variant="code" tone="muted" className="spec-mode-document-credit">
            Settled by {section.credit.by.name}
          </Text>
        ) : null}
        {/* F5 adds the inline cursor flag in this flow slot. */}
        <span className="spec-mode-inline-cursor-slot" aria-hidden="true" />
      </header>

      <NodeViewContent className="spec-mode-section-content" />

      {section.state === "settled" && context.showProvenance && section.provenance.length > 0 ? (
        <div className="spec-mode-provenance-chips" contentEditable={false}>
          {section.provenance.map((source) => (
            <Text as="span" variant="code" tone="muted" key={source.label}>
              {source.label}
            </Text>
          ))}
        </div>
      ) : null}

      {/* Only an OPEN section invites drafting. `isReached` is true for every
          non-open state, so without the state test the invitation came back on
          a section somebody had just excluded — asking them to draft it or
          exclude it again, with the reason they gave stored but never shown.
          It also let "Draft it" sit beside Keep and Revise while a proposed
          section was still empty. */}
      {section.state === "open" && section.isEmpty && section.isReached ? (
        <div className="spec-mode-empty-invitation" contentEditable={false}>
          <Text tone="muted" className="spec-mode-empty-copy">
            Nothing here yet. I can draft this from the three handlers that already touch quota in{" "}
            <Text as="span" variant="code">
              gateway/routes.rs @ 8f2c1a4
            </Text>{" "}
            — or tell me the shape you want and I&apos;ll check it against them.
          </Text>
          <div className="spec-mode-section-actions">
            <Button type="button" size="xs" disabled={pending} onClick={() => setState("proposed")}>
              Draft it
            </Button>
            {section.allowNa ? (
              <NotThisSpecAction
                pending={pending}
                onConfirm={(reason) => setState("n/a", reason)}
              />
            ) : null}
          </div>
        </div>
      ) : null}

      {section.state === "proposed" ? (
        <div className="spec-mode-proposal-actions" contentEditable={false}>
          <div className="spec-mode-section-actions">
            <Button type="button" size="xs" disabled={pending} onClick={() => setState("settled")}>
              Keep
            </Button>
            <Button
              type="button"
              variant="outline"
              size="xs"
              disabled={pending}
              onClick={() => {
                setState("proposed");
                focusBody();
              }}
            >
              Revise
            </Button>
            <Button
              type="button"
              variant="ghost"
              size="xs"
              disabled={pending}
              onClick={() => setState("open")}
            >
              Drop
            </Button>
          </div>
          <Text as="span" variant="code" tone="muted">
            Nobody has blessed this yet
          </Text>
        </div>
      ) : null}
    </NodeViewWrapper>
  );
}

function NotThisSpecAction({
  pending,
  onConfirm,
}: {
  pending: boolean;
  onConfirm: (reason: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [reason, setReason] = useState("");
  const valid = reason.trim().length > 0;
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button type="button" variant="ghost" size="xs" disabled={pending}>
          Not this spec
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="spec-mode-not-this-spec">
        <Text as="label" variant="label" htmlFor="spec-mode-not-this-spec-reason">
          Why does this section not belong in this spec?
        </Text>
        <Textarea
          id="spec-mode-not-this-spec-reason"
          value={reason}
          onChange={(event) => setReason(event.target.value)}
          placeholder="State the reason"
        />
        <Button
          type="button"
          size="sm"
          disabled={!valid}
          onClick={() => {
            if (!valid) return;
            onConfirm(reason.trim());
            setOpen(false);
          }}
        >
          Confirm
        </Button>
      </PopoverContent>
    </Popover>
  );
}
