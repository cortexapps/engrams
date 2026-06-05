import { Outlet, useNavigate } from "@tanstack/react-router";
import { MainSidebar } from "../components/app-sidebar";
import { NewSessionDialog } from "../components/NewSessionDialog";
import { KeyboardShortcuts } from "../keyboard/KeyboardShortcuts";
import { CommandMenu } from "../keyboard/CommandMenu";
import { ShortcutsHelp } from "../keyboard/ShortcutsHelp";
import { useKeyboardUi } from "../keyboard/store";
import { SidebarInset, SidebarProvider, SidebarTrigger } from "@/components/ui/sidebar";

// The app shell: the primary destinations rail + the active surface. Section
// layouts (/sessions, /settings) render their own second sidebar INTO this
// inset's Outlet. Fleet/Storage render full-bleed in the inset.
//
// On desktop there is no top chrome bar — the collapse control lives in the
// primary sidebar's footer. On mobile the primary rail is an off-canvas sheet,
// so a slim trigger-only bar (md:hidden) is the one way to open it.
//
// The keyboard layer lives here, mounted once inside the router: the global
// keymap (KeyboardShortcuts), the ⌘K palette (CommandMenu), the ? cheatsheet
// (ShortcutsHelp), and the single New Session dialog that the `c` key, the
// palette, and the rail button all drive through the keyboard store.
export function RootLayout() {
  const navigate = useNavigate();
  const newSessionOpen = useKeyboardUi((s) => s.newSessionOpen);
  const setNewSessionOpen = useKeyboardUi((s) => s.setNewSessionOpen);

  return (
    <SidebarProvider>
      <MainSidebar />
      <SidebarInset>
        <header className="flex h-12 shrink-0 items-center border-b px-3 md:hidden">
          <SidebarTrigger className="-ml-1" />
        </header>
        <div className="flex flex-1 flex-col">
          <Outlet />
        </div>
      </SidebarInset>

      <KeyboardShortcuts />
      <CommandMenu />
      <ShortcutsHelp />
      <NewSessionDialog
        showTrigger={false}
        open={newSessionOpen}
        onOpenChange={setNewSessionOpen}
        onCreated={(id) => navigate({ to: "/sessions/$id", params: { id } })}
      />
    </SidebarProvider>
  );
}
