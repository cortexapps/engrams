import { Gauge, Boxes, HardDrive, Package, Database } from "lucide-react";
import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import {
  Sidebar,
  SidebarContent,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
} from "@/components/ui/sidebar";
import type { NavItem } from "@/components/nav";
import { cn } from "@/lib/utils";

// The /operator section shell: the admin hat. One section gathers everything
// infrastructure — live telemetry (the Overview cockpit, hosts, storage) and
// the config that telemetry runs on (images, registries) — behind a single
// second rail, in the same two-rail pattern Sessions and Settings use. The
// whole section is admin-gated at the route layer (see router.tsx), so no
// per-item role filtering happens here.
interface Item extends NavItem {
  exact?: boolean;
}

const TELEMETRY: Item[] = [
  { to: "/operator", label: "Overview", icon: Gauge, exact: true },
  { to: "/operator/fleet", label: "Fleet", icon: Boxes },
  { to: "/operator/storage", label: "Storage", icon: HardDrive },
];
const CONFIG: Item[] = [
  { to: "/operator/images", label: "Images", icon: Package },
  { to: "/operator/registries", label: "Registries", icon: Database },
];

const isActive = (pathname: string, it: Item) =>
  it.exact ? pathname === it.to || pathname === `${it.to}/` : pathname.startsWith(it.to as string);

export function OperatorLayout() {
  const pathname = useRouterState({ select: (s) => s.location.pathname });

  const group = (label: string, items: Item[]) => (
    <SidebarGroup key={label}>
      <SidebarGroupLabel>{label}</SidebarGroupLabel>
      <SidebarGroupContent>
        <SidebarMenu>
          {items.map((it) => (
            <SidebarMenuItem key={it.label}>
              <SidebarMenuButton asChild isActive={isActive(pathname, it)}>
                <Link to={it.to}>
                  <it.icon />
                  <span>{it.label}</span>
                </Link>
              </SidebarMenuButton>
            </SidebarMenuItem>
          ))}
        </SidebarMenu>
      </SidebarGroupContent>
    </SidebarGroup>
  );

  const items = [...TELEMETRY, ...CONFIG];

  return (
    <SidebarProvider className="min-h-0 flex-1">
      {/* desktop (md+): vertical second sidebar */}
      <Sidebar
        collapsible="none"
        className="sidebar-section hidden border-r border-sidebar-border md:flex"
      >
        <SidebarContent>
          {group("Telemetry", TELEMETRY)}
          {group("Config", CONFIG)}
        </SidebarContent>
      </Sidebar>
      <div className="flex flex-1 flex-col overflow-auto">
        {/* mobile (<md): horizontal nav strip */}
        <nav className="flex gap-1 overflow-x-auto border-b p-2 md:hidden">
          {items.map((it) => (
            <Link
              key={it.label}
              to={it.to}
              className={cn(
                "inline-flex items-center gap-2 whitespace-nowrap rounded-md px-3 py-1.5 text-sm",
                isActive(pathname, it)
                  ? "bg-accent text-accent-foreground"
                  : "text-muted-foreground",
              )}
            >
              <it.icon className="size-4" />
              {it.label}
            </Link>
          ))}
        </nav>
        <div className="flex-1 p-4 md:p-6">
          <Outlet />
        </div>
      </div>
    </SidebarProvider>
  );
}
