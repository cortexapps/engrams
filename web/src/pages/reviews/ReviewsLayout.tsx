import { Outlet } from "@tanstack/react-router";

import { ReviewsRail } from "./ReviewsRail";
import { Sidebar, SidebarProvider } from "@/components/ui/sidebar";

// The /reviews section shell. Its second sidebar is the persistent ReviewsRail
// (a switcher over reviewed PRs), which stays mounted across the ledger AND the
// dossier — the dossier is a child route, so opening a PR moves the rail's
// highlight instead of swapping the whole layout.
//
// The outlet is layout-neutral: the ledger pads and scrolls itself; the dossier
// fills the height and manages its own panes.
export function ReviewsLayout() {
  return (
    // Bind the section to the viewport height (minus the md:hidden mobile top
    // bar) so the rail keeps its own internal scroll and only the content column
    // moves. `min-h-0` neutralises the provider's base min-h-svh.
    <SidebarProvider className="h-[calc(100svh-3rem)] min-h-0 md:h-svh">
      {/* desktop (md+) only: on a phone the rail would crowd out the thing you
          came to read, and the ledger already lists every PR. */}
      <Sidebar
        collapsible="none"
        className="sidebar-section hidden border-r border-sidebar-border md:flex"
        aria-label="Reviewed pull requests"
      >
        <ReviewsRail />
      </Sidebar>
      <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
        <Outlet />
      </div>
    </SidebarProvider>
  );
}
