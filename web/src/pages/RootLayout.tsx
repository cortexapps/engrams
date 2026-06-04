import { Outlet } from '@tanstack/react-router';
import { MainSidebar } from '../components/app-sidebar';
import { SidebarInset, SidebarProvider, SidebarTrigger } from '@/components/ui/sidebar';

// The app shell: the primary destinations rail + the active surface. Section
// layouts (/sessions, /settings) render their own second sidebar INTO this
// inset's Outlet. Fleet/Storage render full-bleed in the inset.
//
// On desktop there is no top chrome bar — the collapse control lives in the
// primary sidebar's footer. On mobile the primary rail is an off-canvas sheet,
// so a slim trigger-only bar (md:hidden) is the one way to open it.
export function RootLayout() {
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
    </SidebarProvider>
  );
}
