import { Outlet } from '@tanstack/react-router';
import { MainSidebar } from '../components/app-sidebar';
import { SidebarInset, SidebarProvider, SidebarTrigger } from '@/components/ui/sidebar';
import { Separator } from '@/components/ui/separator';

// The app shell: the primary destinations rail + the active surface. Section
// layouts (/sessions, /settings) render their own second sidebar INTO this
// inset's Outlet. Fleet/Storage render full-bleed in the inset.
export function RootLayout() {
  return (
    <SidebarProvider>
      <MainSidebar />
      <SidebarInset>
        <header className="flex h-12 shrink-0 items-center gap-2 border-b px-3">
          <SidebarTrigger className="-ml-1" />
          <Separator orientation="vertical" className="mr-2 h-4" />
        </header>
        <div className="flex flex-1 flex-col">
          <Outlet />
        </div>
      </SidebarInset>
    </SidebarProvider>
  );
}
