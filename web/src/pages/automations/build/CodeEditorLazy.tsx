/** The lazy boundary for CodeMirror (ADR 0119 phase 3.5).
 *
 * `CodeEditor` and the @codemirror packages live in their own chunk; this
 * shim is what the inspector imports, so every other page keeps its bundle.
 * While the chunk loads, a plain read-only textarea holds the source so the
 * layout does not jump. Tests mock THIS module with a textarea double. */

import { lazy, Suspense } from "react";

import type { CodeEditorProps } from "./CodeEditor";

const CodeEditor = lazy(() => import("./CodeEditor"));

export type { CodeEditorProps };

export function CodeEditorLazy(props: CodeEditorProps) {
  return (
    <Suspense
      fallback={
        <textarea
          readOnly
          value={props.value}
          rows={12}
          className="bg-card text-foreground w-full rounded-[10px] border p-2 font-mono text-xs"
          aria-label={props["aria-label"] ?? "Code"}
          aria-busy
        />
      }
    >
      <CodeEditor {...props} />
    </Suspense>
  );
}
