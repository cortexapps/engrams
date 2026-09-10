/** CodeMirror 6 wrapper for the automation code block (ADR 0119 phase 3.5).
 *
 * Loaded lazily (see CodeEditorLazy) so the list page and non-code editing
 * never pay for the editor. Controlled `value`/`onChange`; the EditorView is
 * built once per mount — external value changes become a replace transaction
 * and read-only flips reconfigure a compartment (never a remount).
 *
 * The theme maps straight onto the paper/ink CSS variables from
 * web/src/index.css, so it follows the light/dark theme for free. */

import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { javascript } from "@codemirror/lang-javascript";
import { bracketMatching, indentOnInput, syntaxHighlighting } from "@codemirror/language";
import { HighlightStyle } from "@codemirror/language";
import { lintGutter, setDiagnostics, type Diagnostic } from "@codemirror/lint";
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers } from "@codemirror/view";
import { tags } from "@lezer/highlight";
import { useEffect, useRef } from "react";

export interface CodeEditorProps {
  value: string;
  onChange: (next: string) => void;
  readOnly?: boolean;
  /** 1-based line to squiggle with the error message (EvalCode result). */
  errorLine?: number;
  errorMessage?: string;
  "aria-label"?: string;
  /** Test id for the host element. */
  testId?: string;
}

const paperTheme = EditorView.theme({
  "&": {
    backgroundColor: "var(--card)",
    color: "var(--foreground)",
    border: "1px solid var(--border)",
    borderRadius: "10px",
    fontSize: "12px",
  },
  "&.cm-focused": { outline: "2px solid var(--ring)", outlineOffset: "1px" },
  ".cm-content": {
    fontFamily: "'JetBrains Mono Variable', ui-monospace, 'SF Mono', Menlo, monospace",
    padding: "8px 0",
    caretColor: "var(--foreground)",
  },
  ".cm-scroller": { lineHeight: "1.5", minHeight: "16rem" },
  ".cm-gutters": {
    backgroundColor: "var(--muted)",
    color: "var(--muted-foreground)",
    borderRight: "1px solid var(--border)",
    borderRadius: "10px 0 0 10px",
  },
  ".cm-activeLine": { backgroundColor: "color-mix(in oklch, var(--muted) 60%, transparent)" },
  ".cm-activeLineGutter": { backgroundColor: "var(--muted)" },
  ".cm-selectionBackground, &.cm-focused .cm-selectionBackground": {
    backgroundColor: "color-mix(in oklch, var(--ring) 22%, transparent)",
  },
  ".cm-cursor": { borderLeftColor: "var(--foreground)" },
  ".cm-matchingBracket": { outline: "1px solid var(--ring)" },
  ".cm-lintRange-error": {
    backgroundImage: "none",
    textDecoration: "underline wavy var(--instrument-critical-ink)",
    textUnderlineOffset: "3px",
  },
  ".cm-lint-marker-error": { color: "var(--instrument-critical-ink)" },
  ".cm-tooltip": {
    backgroundColor: "var(--card)",
    color: "var(--foreground)",
    border: "1px solid var(--border)",
    borderRadius: "8px",
  },
});

// Syntax colours stay inside the ink family so the editor reads as one
// surface: strength comes from weight, not hue. The one chromatic accent is
// the ring green on keywords.
const inkHighlight = HighlightStyle.define([
  { tag: tags.keyword, color: "var(--ring)", fontWeight: "600" },
  { tag: [tags.function(tags.variableName), tags.function(tags.propertyName)], fontWeight: "600" },
  { tag: [tags.string, tags.special(tags.string)], color: "var(--muted-foreground)" },
  { tag: tags.number, color: "var(--foreground)", fontWeight: "600" },
  { tag: tags.comment, color: "var(--muted-foreground)", fontStyle: "italic" },
  { tag: tags.propertyName, color: "var(--foreground)" },
]);

function errorDiagnostics(
  state: EditorState,
  errorLine: number | undefined,
  errorMessage: string | undefined,
): Diagnostic[] {
  if (errorLine === undefined || errorMessage === undefined) return [];
  const n = Math.min(Math.max(1, errorLine), state.doc.lines);
  const line = state.doc.line(n);
  return [{ from: line.from, to: line.to, severity: "error", message: errorMessage }];
}

function readOnlyExtensions(readOnly: boolean) {
  return [EditorState.readOnly.of(readOnly), EditorView.editable.of(!readOnly)];
}

export default function CodeEditor(props: CodeEditorProps) {
  const host = useRef<HTMLDivElement | null>(null);
  const view = useRef<EditorView | null>(null);
  const onChangeRef = useRef(props.onChange);
  onChangeRef.current = props.onChange;
  const readOnly = useRef(new Compartment());

  useEffect(() => {
    if (!host.current) return;
    const state = EditorState.create({
      doc: props.value,
      extensions: [
        lineNumbers(),
        history(),
        bracketMatching(),
        indentOnInput(),
        javascript(),
        syntaxHighlighting(inkHighlight),
        keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
        lintGutter(),
        paperTheme,
        readOnly.current.of(readOnlyExtensions(props.readOnly === true)),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) onChangeRef.current(update.state.doc.toString());
        }),
      ],
    });
    const created = new EditorView({ state, parent: host.current });
    view.current = created;
    return () => {
      created.destroy();
      view.current = null;
    };
    // The view is built once per mount; value/readOnly changes are applied
    // through transactions below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const v = view.current;
    if (!v) return;
    const current = v.state.doc.toString();
    if (current !== props.value) {
      v.dispatch({ changes: { from: 0, to: current.length, insert: props.value } });
    }
  }, [props.value]);

  useEffect(() => {
    const v = view.current;
    if (!v) return;
    v.dispatch({
      effects: readOnly.current.reconfigure(readOnlyExtensions(props.readOnly === true)),
    });
  }, [props.readOnly]);

  useEffect(() => {
    // Push the EvalCode error (or its clearing) as diagnostics — no linter
    // source; the only diagnostic this editor ever shows comes from a run.
    const v = view.current;
    if (!v) return;
    v.dispatch(
      setDiagnostics(v.state, errorDiagnostics(v.state, props.errorLine, props.errorMessage)),
    );
  }, [props.errorLine, props.errorMessage, props.value]);

  return (
    <div
      ref={host}
      data-testid={props.testId ?? "code-editor"}
      aria-label={props["aria-label"] ?? "Code"}
      aria-readonly={props.readOnly === true ? true : undefined}
      className="min-w-0"
    />
  );
}
