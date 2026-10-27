/** A template field with inline variable completion.
 *
 * Type `${{` and the list opens at the caret with every path the block can
 * read (trigger, inputs, the event's fields, the outputs of the steps above);
 * keep typing to narrow it, Enter or Tab inserts the path and closes the
 * braces. The list is a plain listbox under the field, so focus never leaves
 * the input. A live preview sits beside each path once a sample has rendered.
 *
 * The "Insert variable" button is the same completion for people who do not
 * know the syntax: it types the opening braces at the caret and opens the
 * list. */

import { Braces } from "lucide-react";
import {
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type ChangeEvent,
  type KeyboardEvent,
} from "react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";

export interface TemplateFieldProps {
  value: string;
  onChange: (next: string) => void;
  multiline?: boolean;
  disabled?: boolean;
  /** Every path the completion offers. */
  paths: readonly string[];
  /** Live previews keyed by path. */
  values?: Readonly<Record<string, string>>;
  placeholder?: string;
  "aria-label"?: string;
  id?: string;
}

/** The open `${{` before the caret and the partial path typed after it. */
const OPEN_RE = /\$\{\{\s*([A-Za-z0-9_.]*)$/;
const MAX_ITEMS = 12;

/** What the completion offers for a partial path: exact prefix matches first,
 * then paths that contain the fragment, each group in the given order. */
export function completionsFor(paths: readonly string[], fragment: string): string[] {
  const needle = fragment.toLowerCase();
  if (needle === "") return paths.slice(0, MAX_ITEMS);
  const starts: string[] = [];
  const contains: string[] = [];
  for (const path of paths) {
    const hay = path.toLowerCase();
    if (hay.startsWith(needle)) starts.push(path);
    else if (hay.includes(needle)) contains.push(path);
    if (starts.length >= MAX_ITEMS) break;
  }
  return [...starts, ...contains].slice(0, MAX_ITEMS);
}

/** Insert `path` for the open `${{` that ends at `caret`, closing the braces
 * unless the text already continues with them. Returns the next text and
 * where the caret lands. */
export function applyCompletion(
  text: string,
  caret: number,
  path: string,
): { text: string; caret: number } {
  const before = text.slice(0, caret);
  const match = OPEN_RE.exec(before);
  if (!match) return { text, caret };
  const start = caret - match[1]!.length;
  const after = text.slice(caret);
  const closes = /^\s*\}\}/.test(after);
  const insert = closes ? path : `${path} }}`;
  const head = `${before.slice(0, start)}${/\s$/.test(before.slice(0, start)) ? "" : " "}`;
  const next = `${head}${insert}${after}`;
  return { text: next, caret: head.length + insert.length };
}

export function TemplateField({
  value,
  onChange,
  multiline,
  disabled,
  paths,
  values,
  placeholder,
  id,
  ...rest
}: TemplateFieldProps) {
  const ref = useRef<HTMLInputElement | HTMLTextAreaElement | null>(null);
  const listId = useId();
  const [caret, setCaret] = useState<number | null>(null);
  const [dismissed, setDismissed] = useState(false);
  const [active, setActive] = useState(0);
  const pendingCaret = useRef<number | null>(null);

  const fragment = useMemo(() => {
    if (caret === null || dismissed || disabled) return null;
    const match = OPEN_RE.exec(value.slice(0, caret));
    return match ? match[1]! : null;
  }, [value, caret, dismissed, disabled]);
  const items = useMemo(
    () => (fragment === null ? [] : completionsFor(paths, fragment)),
    [paths, fragment],
  );
  const open = items.length > 0;

  useEffect(() => setActive(0), [fragment]);
  useEffect(() => {
    // Place the caret after a completion once React has committed the text.
    const el = ref.current;
    if (pendingCaret.current !== null && el) {
      el.setSelectionRange(pendingCaret.current, pendingCaret.current);
      setCaret(pendingCaret.current);
      pendingCaret.current = null;
    }
  }, [value]);

  const syncCaret = () => {
    const el = ref.current;
    if (el) setCaret(el.selectionStart ?? null);
  };
  const handleChange = (e: ChangeEvent<HTMLInputElement | HTMLTextAreaElement>) => {
    setDismissed(false);
    setCaret(e.target.selectionStart ?? null);
    onChange(e.target.value);
  };
  const pick = (path: string) => {
    const at = caret ?? value.length;
    const next = applyCompletion(value, at, path);
    pendingCaret.current = next.caret;
    setDismissed(true);
    onChange(next.text);
    ref.current?.focus();
  };
  const onKeyDown = (e: KeyboardEvent<HTMLInputElement | HTMLTextAreaElement>) => {
    if (!open) return;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive((i) => (i + 1) % items.length);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((i) => (i - 1 + items.length) % items.length);
    } else if (e.key === "Enter" || e.key === "Tab") {
      e.preventDefault();
      pick(items[active]!);
    } else if (e.key === "Escape") {
      e.preventDefault();
      setDismissed(true);
    }
  };
  const insertOpener = () => {
    const el = ref.current;
    const at = el?.selectionStart ?? value.length;
    const head = value.slice(0, at);
    const opener = `${head === "" || /\s$/.test(head) ? "" : " "}\${{ `;
    const next = `${head}${opener}${value.slice(at)}`;
    pendingCaret.current = head.length + opener.length;
    setDismissed(false);
    onChange(next);
    el?.focus();
  };

  const shared = {
    id,
    value,
    disabled,
    placeholder,
    onChange: handleChange,
    onKeyDown,
    onSelect: syncCaret,
    onClick: syncCaret,
    onBlur: () => setDismissed(true),
    role: "combobox" as const,
    "aria-expanded": open,
    "aria-controls": listId,
    "aria-autocomplete": "list" as const,
    "aria-activedescendant": open ? `${listId}-${active}` : undefined,
    "aria-label": rest["aria-label"],
    className: "font-mono text-sm",
  };

  return (
    <div className="relative flex flex-col gap-1">
      {multiline ? (
        <Textarea
          ref={(el) => {
            ref.current = el;
          }}
          rows={5}
          {...shared}
        />
      ) : (
        <Input
          ref={(el) => {
            ref.current = el;
          }}
          {...shared}
        />
      )}
      {open && (
        <ul
          id={listId}
          role="listbox"
          data-testid="variable-completion"
          className="absolute top-full left-0 z-30 mt-1 max-h-64 w-full overflow-y-auto rounded-md border bg-popover p-1 text-popover-foreground shadow-md"
        >
          {items.map((path, index) => (
            <li
              key={path}
              id={`${listId}-${index}`}
              role="option"
              aria-selected={index === active}
              className={cn(
                "flex cursor-default items-baseline justify-between gap-3 rounded px-2 py-1 font-mono text-xs",
                index === active && "bg-accent text-accent-foreground",
              )}
              onMouseDown={(e) => {
                // Before the input blurs, so the pick lands on this field.
                e.preventDefault();
                pick(path);
              }}
              onMouseEnter={() => setActive(index)}
            >
              <span className="truncate">{path}</span>
              {values?.[path] !== undefined && values[path] !== "" && (
                <span className="max-w-[45%] truncate text-muted-foreground">{values[path]}</span>
              )}
            </li>
          ))}
        </ul>
      )}
      {!disabled && (
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="self-start text-muted-foreground"
          onMouseDown={(e) => e.preventDefault()}
          onClick={insertOpener}
        >
          <Braces className="size-3.5" aria-hidden />
          Insert variable
        </Button>
      )}
    </div>
  );
}
