import type { CSSProperties } from "react";
import { Outlet } from "@tanstack/react-router";
import { MainSidebar } from "../components/app-sidebar";
import { KeyboardShortcuts } from "../keyboard/KeyboardShortcuts";
import { CommandMenu } from "../keyboard/CommandMenu";
import { ShortcutsHelp } from "../keyboard/ShortcutsHelp";
import { SidebarInset, SidebarProvider, SidebarTrigger } from "@/components/ui/sidebar";
import { Toaster } from "@/components/ui/sonner";

// The app shell: the spine (products) + the active section. Each section
// renders a SectionLayout — its rail and its lifted sheet — into this inset's
// Outlet.
//
// On desktop there is no top chrome bar: the spine collapses to an icon rail
// from its own edge (SidebarRail) or ⌘B. On mobile the spine is an off-canvas
// sheet, so a slim trigger-only bar (md:hidden) is the one way to open it.
//
// The keyboard layer lives here, mounted once inside the router: the global
// keymap (KeyboardShortcuts), the ⌘K palette (CommandMenu), and the ? cheatsheet
// (ShortcutsHelp). Starting a task is a destination (the /sessions start
// screen), not a modal — `c` and the palette navigate there + focus the composer.

/** The spine's width — 208px. The section rails take the primitive's default. */
const SPINE_WIDTH = "13rem";

export function RootLayout() {
  return (
    // Fixed-height app shell: the wrapper is pinned to the viewport and clips
    // its own overflow, so the spine and each section's rail stay put while
    // only the section's sheet scrolls. Every level down to the Outlet is
    // `min-h-0` so that bound propagates and the section layout's own
    // `overflow-auto` container is what actually scrolls.
    <SidebarProvider
      className="h-svh overflow-hidden"
      style={{ "--sidebar-width": SPINE_WIDTH } as CSSProperties}
    >
      <MainSidebar />
      {/* The inset is the GROUND, not a page: every section lifts its own
          content sheet off it (`.section-sheet`), and the section rail is cut
          from this same tone, so rail and gutter read as one continuous cover
          with the page floating on top. */}
      <SidebarInset className="min-h-0 overflow-hidden bg-shell">
        <header className="flex h-12 shrink-0 items-center border-b border-sidebar-border px-3 text-sidebar-foreground md:hidden">
          <SidebarTrigger className="-ml-1" />
        </header>
        <div className="flex min-h-0 flex-1 flex-col">
          <Outlet />
        </div>
      </SidebarInset>

      <KeyboardShortcuts />
      <CommandMenu />
      <ShortcutsHelp />
      <Toaster />
    </SidebarProvider>
  );
}
