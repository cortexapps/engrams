import { Outlet } from "@tanstack/react-router";

import { ArtifactsRail } from "./ArtifactsRail";
import { Sidebar, SidebarProvider } from "@/components/ui/sidebar";

// The /artifacts section shell — the Reviews/Sessions content-rail shape.
// The rail (recent artifacts) stays mounted across the library AND the
// detail page (a child route), so opening a document moves the highlight
// instead of swapping the layout.
//
// The outlet is layout-neutral: the library pads and scrolls itself; the
// detail page binds to the section height so an HTML artifact's iframe
// can fill it.
export function ArtifactsLayout() {
  return (
    <SidebarProvider className="h-[calc(100svh-3rem)] min-h-0 md:h-svh">
      <Sidebar collapsible="none" className="sidebar-section hidden md:flex" aria-label="Artifacts">
        <ArtifactsRail />
      </Sidebar>
      <div className="section-sheet flex min-w-0 flex-1 flex-col">
        <Outlet />
      </div>
    </SidebarProvider>
  );
}
