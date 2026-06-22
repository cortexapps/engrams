import { Fragment } from "react";
import { useIsAdmin } from "../auth/AuthProvider";
import { useKeyboardUi } from "./store";
import { ALT_LABEL, MOD_LABEL } from "./platform";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Kbd, KbdGroup } from "@/components/ui/kbd";
import { Text } from "@/components/ui/text";

// The `?` cheatsheet — the discoverability home for the keymap. Each row pairs
// a plain-language action with its key caps; sequences read "g then s", chords
// read "⌥ + 1". Modifier glyphs adapt to the platform (⌘ vs Ctrl).

type Cap = { kind: "chord" | "seq" | "range"; keys: string[] };

interface Row {
  label: string;
  cap: Cap;
  admin?: boolean;
}
interface Group {
  heading: string;
  rows: Row[];
}

const chord = (...keys: string[]): Cap => ({ kind: "chord", keys });
const seq = (...keys: string[]): Cap => ({ kind: "seq", keys });
const range = (...keys: string[]): Cap => ({ kind: "range", keys });

const GROUPS: Group[] = [
  {
    heading: "General",
    rows: [
      { label: "Open command palette", cap: chord(MOD_LABEL, "K") },
      { label: "Start new task", cap: chord("c") },
      { label: "Toggle sidebar", cap: chord(MOD_LABEL, "B") },
      { label: "Keyboard shortcuts", cap: chord("?") },
    ],
  },
  {
    heading: "Go to",
    rows: [
      { label: "Tasks", cap: seq("g", "s") },
      { label: "Operator", cap: seq("g", "o"), admin: true },
      { label: "Fleet", cap: seq("g", "f"), admin: true },
      { label: "Settings", cap: seq("g", ",") },
    ],
  },
  {
    heading: "Switch tasks",
    rows: [
      { label: "Jump to task 1–9", cap: range(ALT_LABEL, "1", "9") },
      { label: "Previous / next task", cap: range(ALT_LABEL, "[", "]") },
    ],
  },
];

function CapKeys({ cap }: { cap: Cap }) {
  if (cap.kind === "seq") {
    // "g then s" — a sequence, not a chord.
    return (
      <KbdGroup>
        {cap.keys.map((k, i) => (
          <Fragment key={i}>
            {i > 0 && <span className="text-[0.7rem] text-muted-foreground/70">then</span>}
            <Kbd>{k}</Kbd>
          </Fragment>
        ))}
      </KbdGroup>
    );
  }
  if (cap.kind === "range") {
    // "⌥ 1 … 9" or "⌥ [ / ]".
    const [mod, a, b] = cap.keys;
    const sep = a === "1" ? "…" : "/";
    return (
      <KbdGroup>
        <Kbd>{mod}</Kbd>
        <Kbd>{a}</Kbd>
        <span className="text-[0.7rem] text-muted-foreground/70">{sep}</span>
        <Kbd>{mod}</Kbd>
        <Kbd>{b}</Kbd>
      </KbdGroup>
    );
  }
  return (
    <KbdGroup>
      {cap.keys.map((k, i) => (
        <Kbd key={i}>{k}</Kbd>
      ))}
    </KbdGroup>
  );
}

export function ShortcutsHelp() {
  const open = useKeyboardUi((s) => s.shortcutsOpen);
  const setShortcutsOpen = useKeyboardUi((s) => s.setShortcutsOpen);
  const isAdmin = useIsAdmin();

  return (
    <Dialog open={open} onOpenChange={setShortcutsOpen}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Keyboard shortcuts</DialogTitle>
          <DialogDescription>
            Hold {ALT_LABEL} to reveal each task’s jump number on the rail.
          </DialogDescription>
        </DialogHeader>

        <div className="flex flex-col gap-5">
          {GROUPS.map((group) => {
            const rows = group.rows.filter((r) => !r.admin || isAdmin);
            if (rows.length === 0) return null;
            return (
              <section key={group.heading} className="flex flex-col gap-2">
                <Text as="h3" variant="label" tone="muted">
                  {group.heading}
                </Text>
                <dl className="flex flex-col">
                  {rows.map((row) => (
                    <div
                      key={row.label}
                      className="flex items-center justify-between gap-4 border-b border-border/60 py-2 last:border-0"
                    >
                      <dt className="text-sm text-foreground">{row.label}</dt>
                      <dd>
                        <CapKeys cap={row.cap} />
                      </dd>
                    </div>
                  ))}
                </dl>
              </section>
            );
          })}
        </div>
      </DialogContent>
    </Dialog>
  );
}
