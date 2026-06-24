import { Outlet } from "@tanstack/react-router";
import { MainSidebar } from "../components/app-sidebar";
import { KeyboardShortcuts } from "../keyboard/KeyboardShortcuts";
import { CommandMenu } from "../keyboard/CommandMenu";
import { ShortcutsHelp } from "../keyboard/ShortcutsHelp";
import { SidebarInset, SidebarProvider, SidebarTrigger } from "@/components/ui/sidebar";
import { Toaster } from "@/components/ui/sonner";

// The app shell: the primary destinations rail + the active surface. Section
// layouts (/sessions, /settings) render their own second sidebar INTO this
// inset's Outlet. Fleet/Storage render full-bleed in the inset.
//
// On desktop there is no top chrome bar — the collapse control lives in the
// primary sidebar's footer. On mobile the primary rail is an off-canvas sheet,
// so a slim trigger-only bar (md:hidden) is the one way to open it.
//
// The keyboard layer lives here, mounted once inside the router: the global
// keymap (KeyboardShortcuts), the ⌘K palette (CommandMenu), and the ? cheatsheet
// (ShortcutsHelp). Starting a task is now a destination (the /sessions start
// screen), not a modal — `c` and the palette navigate there + focus the composer.
export function RootLayout() {
  return (
    // Fixed-height app shell: the wrapper is pinned to the viewport and clips
    // its own overflow, so the primary rail and each section's second rail stay
    // put while only the section's content region scrolls. Every level down to
    // the Outlet is `min-h-0` so that bound propagates and the section layout's
    // own `overflow-auto` container is what actually scrolls.
    <SidebarProvider className="h-svh overflow-hidden">
      <MainSidebar />
      <SidebarInset className="min-h-0 overflow-hidden">
        <header className="flex h-12 shrink-0 items-center border-b px-3 md:hidden">
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
