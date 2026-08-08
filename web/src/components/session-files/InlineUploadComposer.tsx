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
      const name = token?.name ?? part.value.split("/").at(-1) ?? part.value;
      const stateClass = token?.status === "error" ? " border-destructive/60 text-destructive" : "";
      const status =
        token?.status === "hashing" || token?.status === "uploading"
          ? '<span class="animate-spin" aria-hidden="true">◌</span>'
          : '<span aria-hidden="true">▱</span>';
      const progress =
        token && token.progress > 0 && token.progress < 1
          ? `<span class="text-muted-foreground">${Math.round(token.progress * 100)}%</span>`
          : "";
      const retry =
        token?.status === "error" && token.file
          ? `<button type="button" data-upload-action="retry" aria-label="Retry ${escapeHtml(name)}">↻</button>`
          : "";
      return `<span contenteditable="false" data-upload-path="${escapeHtml(part.value)}" data-upload-start="${part.start}" data-upload-end="${part.end}" class="mx-0.5 inline-flex max-w-full items-center gap-1 rounded-md border bg-secondary/70 px-1.5 py-0.5 align-baseline font-mono text-xs${stateClass}" title="${escapeHtml(token?.error ?? part.value)}">${status}<span class="max-w-56 truncate">${escapeHtml(name)}</span>${progress}<button type="button" data-upload-action="copy" aria-label="Copy ${escapeHtml(part.value)}">⧉</button>${retry}<button type="button" data-upload-action="remove" aria-label="Remove ${escapeHtml(name)}">×</button></span>`;
    })
    .join("");
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
  const renderedValue = useMemo(() => renderValue(parts, tokensByPath), [parts, tokensByPath]);

  useLayoutEffect(() => {
    const root = rootRef.current;
    if (!root || caretRef.current == null || document.activeElement !== root) return;
    restoreCaret(root, caretRef.current);
    caretRef.current = null;
  }, [renderedValue]);

  const replaceSelection = useCallback(
    (inserted: string) => {
      const root = rootRef.current;
      const offsets = root ? selectionOffsets(root) : null;
      const fallback = caretRef.current ?? value.length;
      const [start, end] = offsets ?? [fallback, fallback];
      caretRef.current = start + inserted.length;
      onChange(value.slice(0, start) + inserted + value.slice(end));
    },
    [onChange, value],
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

  const removePart = (path: string, start: number, end: number) => {
    const next = value.slice(0, start) + value.slice(end);
    caretRef.current = start;
    onChange(next);
    if (!next.includes(path)) {
      const token = tokensByPath.get(path);
      if (token) onRemove(token.id);
    }
  };

  const handlePaste = (event: ClipboardEvent<HTMLDivElement>) => {
    event.preventDefault();
    const pasted = event.clipboardData.getData("text/plain");
    for (const match of pasted.matchAll(new RegExp(CANONICAL_UPLOAD_PATH_SOURCE, "g"))) {
      onCanonicalPath(match[0]);
    }
    replaceSelection(pasted);
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
        onChange(next);
        for (const token of tokens) {
          if (!next.includes(token.path)) onRemove(token.id);
        }
      }}
      onPaste={handlePaste}
      onDragOver={(event) => event.preventDefault()}
      onDrop={handleDrop}
      onKeyDown={(event) => {
        onKeyDown?.(event);
        if (event.defaultPrevented || event.key !== "Enter") return;
        event.preventDefault();
        replaceSelection("\n");
      }}
      onMouseDown={(event) => {
        if ((event.target as HTMLElement).closest("button[data-upload-action]"))
          event.preventDefault();
      }}
      onClick={(event) => {
        const button = (event.target as HTMLElement).closest<HTMLButtonElement>(
          "button[data-upload-action]",
        );
        const chip = button?.closest<HTMLElement>("[data-upload-path]");
        const path = chip?.dataset.uploadPath;
        if (!button || !chip || !path) return;
        const action = button.dataset.uploadAction;
        if (action === "copy") void navigator.clipboard.writeText(path);
        if (action === "retry") {
          const token = tokensByPath.get(path);
          if (token) onRetry(token.id);
        }
        if (action === "remove") {
          removePart(path, Number(chip.dataset.uploadStart), Number(chip.dataset.uploadEnd));
        }
      }}
      dangerouslySetInnerHTML={{ __html: renderedValue }}
    />
  );
});
