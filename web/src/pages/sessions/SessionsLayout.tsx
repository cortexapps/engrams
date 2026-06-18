import { Layers, ListChecks } from "lucide-react";
import { Link, Outlet, useRouterState, type LinkProps } from "@tanstack/react-router";
import { useIsAdmin } from "../../auth/AuthProvider";
import { Sidebar, SidebarProvider } from "@/components/ui/sidebar";
import type { NavItem } from "@/components/nav";
import { SessionsRail } from "./SessionsRail";
import { cn } from "@/lib/utils";

// The /sessions section shell. Its second sidebar is the persistent
// SessionsRail (a live switcher), which stays mounted across the list views
// AND the transcript — SessionDetail is now a child route, so opening a session
// moves the rail's highlight instead of swapping the whole layout. The outlet
// is layout-neutral: each child owns its padding/scroll (the list pages pad +
// scroll; the detail page fills the height with its own panes).
export function SessionsLayout() {
  const isAdmin = useIsAdmin();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const seg = pathname.startsWith("/sessions/") ? pathname.split("/")[2] : undefined;
  const onDetail = !!seg && seg !== "all";

  const scopes: (NavItem & { active: boolean })[] = [
    {
      to: "/sessions",
      label: "My tasks",
      icon: Layers,
      active: pathname === "/sessions" || pathname === "/sessions/",
    },
    ...(isAdmin
      ? [
          {
            to: "/sessions/all" as LinkProps["to"],
            label: "All tasks",
            icon: ListChecks,
            active: pathname.startsWith("/sessions/all"),
          },
        ]
      : []),
  ];

  return (
    // Bind the whole section to the viewport height (minus the mobile top bar,
    // which is `md:hidden`), so BOTH rails stay fixed with their own internal
    // scroll and only the content column scrolls — for the list views and the
    // transcript alike. `min-h-0` neutralises the provider's base `min-h-svh`.
    <SidebarProvider className="h-[calc(100svh-3rem)] min-h-0 md:h-svh">
      {/* desktop (md+): the persistent sessions rail */}
      <Sidebar
        collapsible="none"
        className="sidebar-section hidden border-r border-sidebar-border md:flex"
      >
        <SessionsRail />
      </Sidebar>
      <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
        {/* mobile (<md): horizontal scope strip — the rail is desktop-only, and
            we hide it inside a transcript so it doesn't crowd the detail view. */}
        {!onDetail && (
          <nav className="flex gap-1 overflow-x-auto border-b p-2 md:hidden">
            {scopes.map((it) => (
              <Link
                key={it.label}
                to={it.to}
                className={cn(
                  "inline-flex items-center gap-2 whitespace-nowrap rounded-md px-3 py-1.5 text-sm",
                  it.active ? "bg-accent text-accent-foreground" : "text-muted-foreground",
                )}
              >
                <it.icon className="size-4" />
                {it.label}
              </Link>
            ))}
          </nav>
        )}
        <Outlet />
      </div>
    </SidebarProvider>
  );
}
