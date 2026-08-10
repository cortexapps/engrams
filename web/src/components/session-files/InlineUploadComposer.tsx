import {
  forwardRef,
  useCallback,
  useImperativeHandle,
  useLayoutEffect,
  useMemo,
  useRef,
  type ClipboardEvent,
  type DragEvent,
  type KeyboardEvent,
} from "react";

import { cn } from "@/lib/utils";
import { CANONICAL_UPLOAD_PATH_SOURCE, type UploadToken } from "./useSessionUploads";

interface ValuePart {
  kind: "text" | "upload";
  value: string;
  start: number;
  end: number;
}

function splitValue(value: string): ValuePart[] {
  const parts: ValuePart[] = [];
  const pattern = new RegExp(CANONICAL_UPLOAD_PATH_SOURCE, "g");
  let cursor = 0;
  for (const match of value.matchAll(pattern)) {
    const start = match.index;
    if (start > cursor) {
      parts.push({ kind: "text", value: value.slice(cursor, start), start: cursor, end: start });
    }
    const path = match[0];
    parts.push({ kind: "upload", value: path, start, end: start + path.length });
    cursor = start + path.length;
  }
  if (cursor < value.length) {
    parts.push({ kind: "text", value: value.slice(cursor), start: cursor, end: value.length });
  }
  return parts;
}

function escapeHtml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

function renderValue(parts: readonly ValuePart[], tokensByPath: ReadonlyMap<string, UploadToken>) {
  return parts
    .map((part) => {
      if (part.kind === "text") return escapeHtml(part.value);
      const token = tokensByPath.get(part.value);
      if (!token) return escapeHtml(part.value);
      const name = token?.name ?? part.value.split("/").at(-1) ?? part.value;
      const stateClass =
        token?.status === "error"
          ? " border-destructive/60 bg-destructive/10 text-destructive"
          : " border-border/70 bg-muted/80 text-foreground";
      const status =
        token?.status === "hashing" || token?.status === "uploading"
          ? '<span class="animate-spin" aria-hidden="true">◌</span>'
          : "";
      const progress =
        token && token.progress > 0 && token.progress < 1
          ? `<span class="text-muted-foreground">${Math.round(token.progress * 100)}%</span>`
          : "";
      const retry =
        token?.status === "error" && token.file
          ? `<button type="button" contenteditable="false" data-upload-action="retry" class="-mr-0.5 rounded px-0.5 hover:bg-destructive/15" aria-label="Retry ${escapeHtml(name)}">↻</button>`
          : "";
      return `<span contenteditable="false" data-upload-path="${escapeHtml(part.value)}" aria-label="${escapeHtml(name)} attachment" class="mx-0.5 inline-flex max-w-full cursor-default select-all items-center gap-1 rounded-md border px-1.5 py-px align-baseline font-mono text-[0.82em] leading-5 shadow-sm transition-colors selection:bg-primary/25${stateClass}" title="${escapeHtml(token?.error ?? part.value)}">${status}<span class="max-w-56 truncate">${escapeHtml(name)}</span>${progress}${retry}</span>`;
    })
    .join("");
}

// Round-trip our markup through the parser so it can be compared against a
// live `innerHTML` on equal terms: typing a space leaves U+00A0 in the DOM
// (serialized `&nbsp;`), a typed quote stays literal where `escapeHtml` writes
// `&quot;`, and so on. Both sides then serialize identically, so the sync below
// only rewrites when the editor genuinely does not show the value.
let scratch: HTMLDivElement | null = null;
function normalizeHtml(html: string): string {
  scratch ??= document.createElement("div");
  scratch.innerHTML = html;
  return scratch.innerHTML;
}

function nodeValue(node: Node): string {
  if (node.nodeType === Node.TEXT_NODE) return node.textContent ?? "";
  if (!(node instanceof HTMLElement)) return "";
  const uploadPath = node.dataset.uploadPath;
  if (uploadPath) return uploadPath;
  if (node.tagName === "BR") return "\n";
  return Array.from(node.childNodes, nodeValue).join("");
}

function editorValue(root: HTMLElement): string {
  const nodes = Array.from(root.childNodes);
  let value = "";
  for (const [index, node] of nodes.entries()) {
    value += nodeValue(node);
    if (!(node instanceof HTMLElement) || !node.dataset.uploadPath) continue;
    const next = nodes[index + 1];
    if (next && /^[A-Za-z0-9._-]/.test(nodeValue(next))) value += " ";
  }
  return value;
}

function nodeLength(node: Node): number {
  return nodeValue(node).length;
}

function offsetWithin(root: HTMLElement, target: Node, targetOffset: number): number | null {
  let total = 0;
  let found: number | null = null;
  const visit = (node: Node) => {
    if (found != null) return;
    if (node === target) {
      if (node.nodeType === Node.TEXT_NODE) {
        found = total + Math.min(targetOffset, node.textContent?.length ?? 0);
      } else {
        found =
          total +
          Array.from(node.childNodes)
            .slice(0, targetOffset)
            .reduce((sum, child) => sum + nodeLength(child), 0);
      }
      return;
    }
    if (node instanceof HTMLElement && node.dataset.uploadPath) {
      total += node.dataset.uploadPath.length;
      return;
    }
    if (
      node.nodeType === Node.TEXT_NODE ||
      (node instanceof HTMLElement && node.tagName === "BR")
    ) {
      total += nodeLength(node);
      return;
    }
    for (const child of Array.from(node.childNodes)) visit(child);
  };
  visit(root);
  return found;
}

function selectionOffsets(root: HTMLElement): [number, number] | null {
  const selection = window.getSelection();
  if (!selection?.anchorNode || !selection.focusNode) return null;
  if (!root.contains(selection.anchorNode) || !root.contains(selection.focusNode)) return null;
  const anchor = offsetWithin(root, selection.anchorNode, selection.anchorOffset);
  const focus = offsetWithin(root, selection.focusNode, selection.focusOffset);
  if (anchor == null || focus == null) return null;
  return anchor <= focus ? [anchor, focus] : [focus, anchor];
}

function restoreCaret(root: HTMLElement, offset: number) {
  const selection = window.getSelection();
  if (!selection) return;
  const range = document.createRange();
  let remaining = offset;
  for (const node of Array.from(root.childNodes)) {
    const length = nodeLength(node);
    if (node.nodeType === Node.TEXT_NODE && remaining <= length) {
      range.setStart(node, remaining);
      range.collapse(true);
      selection.removeAllRanges();
      selection.addRange(range);
      return;
    }
    if (node instanceof HTMLElement && node.dataset.uploadPath && remaining <= length) {
      if (remaining === 0) range.setStartBefore(node);
      else range.setStartAfter(node);
      range.collapse(true);
      selection.removeAllRanges();
      selection.addRange(range);
      return;
    }
    remaining -= length;
  }
  range.selectNodeContents(root);
  range.collapse(false);
  selection.removeAllRanges();
  selection.addRange(range);
}

function selectUploadToken(root: HTMLElement, token: HTMLElement): number | null {
  const selection = window.getSelection();
  if (!selection) return null;
  const range = document.createRange();
  range.selectNode(token);
  selection.removeAllRanges();
  selection.addRange(range);
  return selectionOffsets(root)?.[1] ?? null;
}

export interface InlineUploadComposerHandle {
  addFiles: (files: FileList | readonly File[]) => void;
  focus: () => void;
}

export interface InlineUploadComposerProps {
  value: string;
  tokens: readonly UploadToken[];
  onChange: (value: string) => void;
  onFiles: (files: FileList | readonly File[]) => UploadToken[];
  onCanonicalPath: (path: string) => boolean;
  onRemove: (id: string) => void;
  onRetry: (id: string) => void;
  onKeyDown?: (event: KeyboardEvent<HTMLDivElement>) => void;
  placeholder: string;
  ariaLabel: string;
  disabled?: boolean;
  className?: string;
}

export const InlineUploadComposer = forwardRef<
  InlineUploadComposerHandle,
  InlineUploadComposerProps
>(function InlineUploadComposer(
  {
    value,
    tokens,
    onChange,
    onFiles,
    onCanonicalPath,
    onRemove,
    onRetry,
    onKeyDown,
    placeholder,
    ariaLabel,
    disabled,
    className,
  },
  forwardedRef,
) {
  const rootRef = useRef<HTMLDivElement>(null);
  const caretRef = useRef<number | null>(null);
  const parts = useMemo(() => splitValue(value), [value]);
  const tokensByPath = useMemo(() => new Map(tokens.map((token) => [token.path, token])), [tokens]);
  const renderedValue = useMemo(
    () => normalizeHtml(renderValue(parts, tokensByPath)),
    [parts, tokensByPath],
  );

  // The edit we last reported and have not yet seen echoed back through
  // `value`. Until the echo lands, every incoming `value` is a render from
  // BEFORE that edit — older than what the editor already shows — so the sync
  // below must not write it back.
  const pendingRef = useRef<string | null>(null);

  const emit = useCallback(
    (next: string) => {
      pendingRef.current = next;
      onChange(next);
    },
    [onChange],
  );

  const removeMissingTokens = useCallback(
    (next: string) => {
      for (const token of tokens) {
        if (!next.includes(token.path)) onRemove(token.id);
      }
    },
    [onRemove, tokens],
  );

  const replaceRange = useCallback(
    (start: number, end: number, inserted: string) => {
      const next = value.slice(0, start) + inserted + value.slice(end);
      caretRef.current = start + inserted.length;
      emit(next);
      removeMissingTokens(next);
    },
    [emit, removeMissingTokens, value],
  );

  // React must NOT own this subtree. React 19 assigns `innerHTML`
  // UNCONDITIONALLY on every commit that re-renders an element carrying
  // `dangerouslySetInnerHTML` (the `{__html}` literal is a new object each
  // render, so the identity bailout never applies, and `setProp` does no
  // string compare) — and replacing the children of a focused contenteditable
  // collapses the caret to offset 0. The session transcript re-renders this
  // subtree on every streamed token, so an unrelated render threw the caret to
  // the start of the composer many times a second while the user typed.
  //
  // So we write the markup ourselves, only when the editor does not already
  // show it, and restore the caret exactly when we did rewrite. A re-render
  // that changes nothing here now leaves the editor's DOM — and the caret —
  // alone.
  useLayoutEffect(() => {
    const root = rootRef.current;
    if (!root) return;
    if (pendingRef.current != null) {
      // Still waiting on our own edit: this render predates it. Writing now
      // would revert the keystrokes the browser has already applied — the
      // commit that carries the echo arrives right behind this one.
      if (value !== pendingRef.current) return;
      pendingRef.current = null;
    }
    if (root.innerHTML === renderedValue) return;
    root.innerHTML = renderedValue;
    if (caretRef.current == null || document.activeElement !== root) return;
    restoreCaret(root, caretRef.current);
  });

  const replaceSelection = useCallback(
    (inserted: string) => {
      const root = rootRef.current;
      const offsets = root ? selectionOffsets(root) : null;
      const fallback = caretRef.current ?? value.length;
      const [start, end] = offsets ?? [fallback, fallback];
      replaceRange(start, end, inserted);
    },
    [replaceRange, value.length],
  );

  const attachFiles = useCallback(
    (files: FileList | readonly File[]) => {
      const added = onFiles(files);
      if (added.length > 0) replaceSelection(`${added.map((token) => token.path).join(" ")} `);
    },
    [onFiles, replaceSelection],
  );

  useImperativeHandle(
    forwardedRef,
    () => ({ addFiles: attachFiles, focus: () => rootRef.current?.focus() }),
    [attachFiles],
  );

  const handlePaste = (event: ClipboardEvent<HTMLDivElement>) => {
    event.preventDefault();
    const pasted = event.clipboardData.getData("text/plain");
    for (const match of pasted.matchAll(new RegExp(CANONICAL_UPLOAD_PATH_SOURCE, "g"))) {
      onCanonicalPath(match[0]);
    }
    replaceSelection(pasted);
  };

  const handleCopy = (event: ClipboardEvent<HTMLDivElement>) => {
    const root = rootRef.current;
    const offsets = root ? selectionOffsets(root) : null;
    if (!offsets || offsets[0] === offsets[1]) return;
    event.preventDefault();
    event.clipboardData.setData("text/plain", value.slice(offsets[0], offsets[1]));
  };

  const handleCut = (event: ClipboardEvent<HTMLDivElement>) => {
    const root = rootRef.current;
    const offsets = root ? selectionOffsets(root) : null;
    if (!offsets || offsets[0] === offsets[1] || disabled) return;
    event.preventDefault();
    event.clipboardData.setData("text/plain", value.slice(offsets[0], offsets[1]));
    replaceRange(offsets[0], offsets[1], "");
  };

  const handleDrop = (event: DragEvent<HTMLDivElement>) => {
    if (event.dataTransfer.files.length === 0) return;
    event.preventDefault();
    attachFiles(event.dataTransfer.files);
  };

  return (
    <div
      ref={rootRef}
      contentEditable={!disabled}
      role="textbox"
      aria-label={ariaLabel}
      aria-multiline="true"
      data-placeholder={placeholder}
      className={cn(
        "max-h-40 min-h-9 flex-1 overflow-y-auto whitespace-pre-wrap bg-transparent outline-none empty:before:pointer-events-none empty:before:text-muted-foreground/80 empty:before:content-[attr(data-placeholder)]",
        className,
      )}
      onFocus={() => {
        if (caretRef.current != null && rootRef.current)
          restoreCaret(rootRef.current, caretRef.current);
      }}
      onSelect={() => {
        if (!rootRef.current) return;
        const offsets = selectionOffsets(rootRef.current);
        if (offsets) caretRef.current = offsets[1];
      }}
      onInput={() => {
        const root = rootRef.current;
        if (!root) return;
        const offsets = selectionOffsets(root);
        if (offsets) caretRef.current = offsets[1];
        const next = editorValue(root);
        emit(next);
        removeMissingTokens(next);
      }}
      onCopy={handleCopy}
      onCut={handleCut}
      onPaste={handlePaste}
      onDragOver={(event) => event.preventDefault()}
      onDrop={handleDrop}
      onKeyDown={(event) => {
        onKeyDown?.(event);
        if (event.defaultPrevented) return;
        if (event.key === "Backspace" || event.key === "Delete") {
          const root = rootRef.current;
          const offsets = root ? selectionOffsets(root) : null;
          if (offsets) {
            let [start, end] = offsets;
            if (start === end) {
              const adjacent = parts.find(
                (part) =>
                  part.kind === "upload" &&
                  (event.key === "Backspace" ? part.end === start : part.start === start),
              );
              if (!adjacent) return;
              start = adjacent.start;
              end = adjacent.end;
            }
            event.preventDefault();
            replaceRange(start, end, "");
          }
          return;
        }
        if (event.key !== "Enter") return;
        event.preventDefault();
        replaceSelection("\n");
      }}
      onMouseDown={(event) => {
        const target = event.target as HTMLElement;
        if (target.closest("button[data-upload-action]")) {
          event.preventDefault();
          return;
        }
        const chip = target.closest<HTMLElement>("[data-upload-path]");
        const root = rootRef.current;
        if (!chip || !root) return;
        event.preventDefault();
        root.focus();
        caretRef.current = selectUploadToken(root, chip);
      }}
      onClick={(event) => {
        const button = (event.target as HTMLElement).closest<HTMLButtonElement>(
          "button[data-upload-action]",
        );
        const chip = button?.closest<HTMLElement>("[data-upload-path]");
        const path = chip?.dataset.uploadPath;
        if (!button || !chip || !path) return;
        if (button.dataset.uploadAction === "retry") {
          const token = tokensByPath.get(path);
          if (token) onRetry(token.id);
        }
      }}
      // No children and no `dangerouslySetInnerHTML`: the markup inside is
      // written by the layout effect above, never by React.
    />
  );
});
