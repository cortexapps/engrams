import { KeyRound, Lock, Plug, Users, UserCircle, IdCard, Cpu } from "lucide-react";
import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import { useAbility } from "../../auth/AuthProvider";
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

// Account scope only. Infrastructure config (images, registries) moved to the
// Operator section; Settings now holds your own account and org-wide membership.
const MINE: NavItem[] = [
  { to: "/settings/profile", label: "Profile", icon: UserCircle },
  { to: "/settings/tokens", label: "Tokens", icon: KeyRound },
];
const ORG: NavItem[] = [
  { to: "/settings/members", label: "Members", icon: Users },
  { to: "/settings/secrets", label: "Secrets", icon: Lock },
  { to: "/settings/integrations", label: "Integrations", icon: Plug },
  { to: "/settings/harnesses", label: "Harnesses", icon: Cpu },
  { to: "/settings/profiles", label: "Profiles", icon: IdCard },
];

export function SettingsLayout() {
  const ability = useAbility();
  const isAdmin = ability.can("manage", "all");
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const group = (label: string, items: NavItem[]) => (
    <SidebarGroup key={label}>
      <SidebarGroupLabel>{label}</SidebarGroupLabel>
      <SidebarGroupContent>
        <SidebarMenu>
          {items.map((it) => (
            <SidebarMenuItem key={it.label}>
              <SidebarMenuButton asChild isActive={pathname.startsWith(it.to as string)}>
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

  const items = [...MINE, ...(isAdmin ? ORG : [])];

  return (
    <SidebarProvider className="min-h-0 flex-1">
      {/* desktop (md+): vertical second sidebar */}
      <Sidebar
        collapsible="none"
        className="sidebar-section hidden border-r border-sidebar-border md:flex"
      >
        <SidebarContent>
          {group("My settings", MINE)}
          {isAdmin && group("Org", ORG)}
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
                pathname.startsWith(it.to as string)
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
