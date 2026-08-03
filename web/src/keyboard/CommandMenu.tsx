import {
  Box,
  Database,
  FileBox,
  Gauge,
  KeyRound,
  KeySquare,
  Layers,
  Server,
  SquarePlus,
  User,
  Users,
} from "lucide-react";
import { useNavigate } from "@tanstack/react-router";
import type { LinkProps } from "@tanstack/react-router";
import type { LucideIcon } from "lucide-react";
import { useIsAdmin } from "../auth/AuthProvider";
import { StatusGlyph } from "../components/Glyph";
import { shortId, stripImageHost } from "../pages/sessions/session-format";
import { useRailSessions } from "../pages/sessions/useRailSessions";
import { useKeyboardUi } from "./store";
import { ALT_LABEL } from "./platform";
import {
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandSeparator,
  CommandShortcut,
} from "@/components/ui/command";
import { Kbd, KbdGroup } from "@/components/ui/kbd";

// The ⌘K palette. No backend search: the input filters locally over the
// already-loaded recent sessions plus the static Actions / Go-to commands
// (cmdk's built-in substring scoring). Three groups, in the order a developer
// reaches for them: act, switch, navigate.

interface Dest {
  to: LinkProps["to"];
  label: string;
  icon: LucideIcon;
  admin?: boolean;
  /** g-leader hint, when one exists. */
  leader?: string;
}

// `value` strings (what cmdk scores against) deliberately include extra synonyms
// so "go to fleet" / "hosts" style typing still lands.
const DESTS: Dest[] = [
  { to: "/sessions", label: "Tasks", icon: Layers, leader: "s" },
  { to: "/artifacts", label: "Artifacts", icon: FileBox, leader: "a" },
  { to: "/operator", label: "Operator", icon: Gauge, admin: true, leader: "o" },
  { to: "/operator/fleet", label: "Fleet", icon: Server, admin: true, leader: "f" },
  { to: "/operator/storage", label: "Storage", icon: Database, admin: true },
  { to: "/operator/images", label: "Images", icon: Box, admin: true },
  { to: "/operator/registries", label: "Registries", icon: KeyRound, admin: true },
  { to: "/settings", label: "Settings · Profile", icon: User, leader: "," },
  { to: "/settings/credentials", label: "Settings · Credentials", icon: KeySquare },
  { to: "/settings/members", label: "Settings · Members", icon: Users, admin: true },
];

export function CommandMenu() {
  const open = useKeyboardUi((s) => s.paletteOpen);
  const setPaletteOpen = useKeyboardUi((s) => s.setPaletteOpen);
  const requestComposerFocus = useKeyboardUi((s) => s.requestComposerFocus);
  const navigate = useNavigate();
  const isAdmin = useIsAdmin();
  const { rows, isPending, error } = useRailSessions();

  // Close the palette first, then run — so focus restores from the palette
  // before a navigation paints or the New Session dialog grabs the focus trap.
  const run = (action: () => void) => {
    setPaletteOpen(false);
    action();
  };

  const dests = DESTS.filter((d) => !d.admin || isAdmin);

  return (
    <CommandDialog open={open} onOpenChange={setPaletteOpen}>
      <CommandInput placeholder="Type a command or task…" />
      <CommandList>
        <CommandEmpty>No matching commands.</CommandEmpty>

        <CommandGroup heading="Actions">
          <CommandItem
            value="start new task create launch"
            onSelect={() =>
              run(() => {
                navigate({ to: "/sessions" });
                requestComposerFocus();
              })
            }
          >
            <SquarePlus />
            <span>Start new task</span>
            <CommandShortcut>
              <Kbd>c</Kbd>
            </CommandShortcut>
          </CommandItem>
        </CommandGroup>

        {(isPending || error || rows.length > 0) && (
          <>
            <CommandSeparator />
            <CommandGroup heading="Tasks">
              {isPending ? (
                <CommandItem disabled value="loading tasks">
                  Loading tasks…
                </CommandItem>
              ) : error ? (
                <CommandItem disabled value="tasks error">
                  Couldn’t load tasks.
                </CommandItem>
              ) : (
                rows.map((r, i) => (
                  <CommandItem
                    key={r.id}
                    value={`task ${shortId(r.id)} ${stripImageHost(r.image)} ${r.id}`}
                    onSelect={() =>
                      run(() => navigate({ to: "/sessions/$id", params: { id: r.id } }))
                    }
                  >
                    <span className="text-[0.7rem] leading-none">
                      <StatusGlyph status={r.status} beat={false} />
                    </span>
                    <span className="flex min-w-0 flex-1 items-baseline gap-2">
                      <span className="truncate font-mono text-[0.8rem]">{shortId(r.id)}</span>
                      <span className="truncate text-xs text-muted-foreground">
                        {stripImageHost(r.image)}
                      </span>
                    </span>
                    {i < 9 && (
                      <CommandShortcut>
                        <KbdGroup>
                          <Kbd>{ALT_LABEL}</Kbd>
                          <Kbd>{i + 1}</Kbd>
                        </KbdGroup>
                      </CommandShortcut>
                    )}
                  </CommandItem>
                ))
              )}
            </CommandGroup>
          </>
        )}

        <CommandSeparator />
        <CommandGroup heading="Go to">
          {dests.map((d) => (
            <CommandItem
              key={d.to}
              value={`go to ${d.label}`}
              onSelect={() => run(() => navigate({ to: d.to }))}
            >
              <d.icon />
              <span>{d.label}</span>
              {d.leader && (
                <CommandShortcut>
                  <KbdGroup>
                    <Kbd>g</Kbd>
                    <Kbd>{d.leader}</Kbd>
                  </KbdGroup>
                </CommandShortcut>
              )}
            </CommandItem>
          ))}
        </CommandGroup>
      </CommandList>
    </CommandDialog>
  );
}
