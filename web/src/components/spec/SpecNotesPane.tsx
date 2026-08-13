import { useEffect, useMemo, useRef, useState } from "react";
import { Collaboration } from "@tiptap/extension-collaboration";
import { CollaborationCaret } from "@tiptap/extension-collaboration-caret";
import { EditorContent, useEditor, type Editor } from "@tiptap/react";
import {
  readWorkingNotes,
  SPEC_NOTES_FRAGMENT_NAME,
  untaggedBulletCount,
} from "@engrams/spec-document";
import type { WebsocketProvider } from "y-websocket";
import type * as Y from "yjs";

import { Button } from "@/components/ui/button";
import { specNotesExtensions } from "./notes-extensions";
import "./spec-notes.css";

export interface SpecNotesSummary {
  untaggedBullets: number;
  totalBullets: number;
  contradictedBullets: number;
}

/** Which way the untagged pile is moving. R22's "keep talking" gauge. */
export type SpecNotesTrend = "growing" | "shrinking";

/**
 * The working notes on the canvas (ADR 0114 D6, R21-R23).
 *
 * The pane reads as scratch paper on purpose: ruled lines and a dashed border
 * say that this is not the spec. A person edits the words in any bullet, which
 * corrects the agent without a chat round-trip; the marks, the receipts and the
 * destination tags stay the agent's model. After distillation the same pane is
 * the read-only archive.
 */
export function SpecNotesPane({
  doc,
  provider,
  user,
  archived,
  onDistill,
  distilling = false,
  distillError = null,
}: {
  doc: Y.Doc;
  provider: WebsocketProvider;
  user: { name: string; color: string };
  archived: boolean;
  /** Omit it when this viewer cannot close the stage. */
  onDistill?: () => void;
  distilling?: boolean;
  distillError?: string | null;
}) {
  const extensions = useMemo(
    () => [
      ...specNotesExtensions,
      Collaboration.configure({ document: doc, field: SPEC_NOTES_FRAGMENT_NAME }),
      CollaborationCaret.configure({ provider, user }),
    ],
    [doc, provider, user],
  );
  const editor = useEditor({
    extensions,
    immediatelyRender: false,
    editable: !archived,
    editorProps: {
      attributes: {
        class: "spec-notes-editor",
        "aria-label": archived ? "Notes archive text" : "Working notes text",
      },
    },
  });
  const summary = useNotesSummary(editor);
  const trend = useUntaggedTrend(summary?.untaggedBullets ?? null);

  return (
    <section className="spec-notes-pane" aria-label={archived ? "Notes archive" : "Working notes"}>
      <header className="spec-notes-bar">
        <span className="spec-notes-label">
          {archived ? "Notes archive — not the spec" : "Working notes — not the spec"}
        </span>
        {summary && !archived && (
          <span className="spec-notes-gauge">{gaugeText(summary.untaggedBullets, trend)}</span>
        )}
        {archived && (
          <span className="spec-notes-gauge">distilled · read-only · never published</span>
        )}
        {onDistill && !archived && (
          <Button type="button" size="sm" disabled={distilling} onClick={onDistill}>
            {distilling ? "Drafting…" : "Draft the spec"}
          </Button>
        )}
      </header>
      {distillError !== null && <p className="spec-notes-error">{distillError}</p>}
      {editor && <EditorContent editor={editor} />}
    </section>
  );
}

export function gaugeText(untagged: number, trend: SpecNotesTrend | null): string {
  return `untagged pile: ${untagged}${trend === null ? "" : ` · ${trend}`}`;
}

/** Read the gauge from the editor's own document, so it moves with a keystroke. */
export function useNotesSummary(editor: Editor | null): SpecNotesSummary | null {
  const [summary, setSummary] = useState<SpecNotesSummary | null>(null);
  useEffect(() => {
    if (!editor) return;
    const read = () => setSummary(summarizeNotes(editor));
    read();
    editor.on("update", read);
    editor.on("transaction", read);
    return () => {
      editor.off("update", read);
      editor.off("transaction", read);
    };
  }, [editor]);
  return summary;
}

export function summarizeNotes(editor: Editor): SpecNotesSummary | null {
  let notes;
  try {
    notes = readWorkingNotes(editor.state.doc);
  } catch {
    // The binding has not delivered the server's notes yet.
    return null;
  }
  const bullets = notes.clusters.flatMap((cluster) => cluster.bullets);
  return {
    untaggedBullets: untaggedBulletCount(notes),
    totalBullets: bullets.length,
    contradictedBullets: bullets.filter((bullet) => bullet.mark === "contradicted").length,
  };
}

/**
 * The trend this browser has seen. It reports the movement of the pile, so it
 * claims nothing about history that this session did not watch.
 */
export function useUntaggedTrend(untagged: number | null): SpecNotesTrend | null {
  const previous = useRef<number | null>(null);
  const [trend, setTrend] = useState<SpecNotesTrend | null>(null);
  useEffect(() => {
    if (untagged === null) return;
    const before = previous.current;
    previous.current = untagged;
    if (before === null || before === untagged) return;
    setTrend(untagged > before ? "growing" : "shrinking");
  }, [untagged]);
  return trend;
}
