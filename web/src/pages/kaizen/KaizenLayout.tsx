import { ListChecks } from "lucide-react";
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

const ITEMS: NavItem[] = [{ to: "/kaizen/papercuts", label: "Papercuts", icon: ListChecks }];

export function KaizenLayout() {
  const pathname = useRouterState({ select: (s) => s.location.pathname });

  return (
    <SidebarProvider className="min-h-0 flex-1">
      <Sidebar collapsible="none" className="sidebar-section hidden md:flex">
        <SidebarContent>
          <SidebarGroup>
            <SidebarGroupLabel>Kaizen</SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu>
                {ITEMS.map((item) => (
                  <SidebarMenuItem key={item.label}>
                    <SidebarMenuButton asChild isActive={pathname.startsWith(item.to as string)}>
                      <Link to={item.to}>
                        <item.icon />
                        <span>{item.label}</span>
                      </Link>
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        </SidebarContent>
      </Sidebar>

      <div className="section-sheet flex flex-1 flex-col overflow-y-auto">
        <nav className="flex gap-1 overflow-x-auto border-b p-2 md:hidden">
          {ITEMS.map((item) => (
            <Link
              key={item.label}
              to={item.to}
              className={cn(
                "inline-flex items-center gap-2 whitespace-nowrap rounded-md px-3 py-1.5 text-sm",
                pathname.startsWith(item.to as string)
                  ? "bg-accent text-accent-foreground"
                  : "text-muted-foreground",
              )}
            >
              <item.icon className="size-4" />
              {item.label}
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
