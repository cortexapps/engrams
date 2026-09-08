import type { ReactNode } from "react";
import { Link, useRouterState, type LinkProps } from "@tanstack/react-router";
import type { LucideIcon } from "lucide-react";

import { Sidebar, SidebarProvider, SidebarResizeHandle } from "@/components/ui/sidebar";
import { cn } from "@/lib/utils";

// Every product section is the same three columns: the spine (RootLayout), a
// section rail cut from the cover, and the page lifted off the cover as one
// sheet. This is the ONE place that shape lives. A section supplies its rail
// and its content; it does not rebuild the frame.
//
// The provider binds the section to the viewport height (minus the md:hidden
// mobile top bar) so the rail keeps its own internal scroll and only the
// content column moves. `min-h-0` neutralises the provider's base `min-h-svh`.

/** The section rail's width — 272px. The spine sets its own in RootLayout. */
export const SECTION_RAIL_WIDTH = "17rem";

export interface SectionNavItem {
  to: LinkProps["to"];
  label: string;
  icon?: LucideIcon;
  active: boolean;
}

export function SectionLayout({
  rail,
  railLabel,
  resizeStorageKey,
  resizeLabel,
  nav,
  sheet = true,
  contentClassName,
  children,
}: {
  /** The rail's content (header / content / footer primitives). */
  rail: ReactNode;
  /** Accessible name for the rail region. */
  railLabel: string;
  /** Present when the rail is drag-resizable (the Tasks rail). */
  resizeStorageKey?: string;
  resizeLabel?: string;
  /** The mobile (<md) strip — the rail is desktop-only. Omit when the section
   * has no destinations worth a strip (the link rails). */
  nav?: SectionNavItem[];
  /** False when the child builds its own sheets (a transcript's two panes). */
  sheet?: boolean;
  contentClassName?: string;
  children: ReactNode;
}) {
  return (
    <SidebarProvider
      className="h-[calc(100svh-3rem)] min-h-0 md:h-svh"
      style={{ "--sidebar-width": SECTION_RAIL_WIDTH } as React.CSSProperties}
    >
      <Sidebar
        collapsible="none"
        className="sidebar-section hidden shrink-0 md:flex"
        aria-label={railLabel}
      >
        {rail}
        {resizeStorageKey && (
          <SidebarResizeHandle storageKey={resizeStorageKey} label={resizeLabel} />
        )}
      </Sidebar>
      <div
        className={cn(
          "flex min-w-0 flex-1 flex-col overflow-hidden",
          sheet && "section-sheet",
          contentClassName,
        )}
      >
        {nav && <SectionNav items={nav} />}
        {children}
      </div>
    </SidebarProvider>
  );
}

/** The mobile destination strip. One component, so the sections cannot drift. */
export function SectionNav({ items, className }: { items: SectionNavItem[]; className?: string }) {
  return (
    <nav className={cn("flex shrink-0 gap-1 overflow-x-auto border-b p-2 md:hidden", className)}>
      {items.map((it) => (
        <Link
          key={it.label}
          to={it.to}
          className={cn(
            "inline-flex items-center gap-2 whitespace-nowrap rounded-md px-3 py-1.5 text-sm",
            it.active ? "bg-accent text-accent-foreground" : "text-muted-foreground",
          )}
        >
          {it.icon && <it.icon className="size-4" />}
          {it.label}
        </Link>
      ))}
    </nav>
  );
}

/** The padded, scrolling body of a nav-rail section (Settings, Automations).
 * The switcher-rail sections (Tasks, Reviews, Artifacts) leave padding to the
 * page, because a transcript or a dossier fills the height itself. */
export function SectionPage({ children }: { children: ReactNode }) {
  // Keyed on the path so a route change re-mounts the body and it rises in
  // (240ms); the rails around it never move.
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  return (
    <div className="min-h-0 flex-1 overflow-y-auto">
      <div key={pathname} className="page-in p-4 md:px-9 md:pt-7 md:pb-8">
        {children}
      </div>
    </div>
  );
}
