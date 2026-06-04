import { Boxes, HardDrive, Layers, Settings } from 'lucide-react';
import { Link, useRouterState, type LinkProps } from '@tanstack/react-router';
import { useIsAdmin } from '../auth/AuthProvider';
import { EngramMark } from './EngramMark';
import { ModeToggle } from './mode-toggle';
import { UserMenu } from './user-menu';
import {
  Sidebar, SidebarContent, SidebarFooter, SidebarGroup, SidebarGroupContent,
  SidebarHeader, SidebarMenu, SidebarMenuButton, SidebarMenuItem, SidebarTrigger,
} from '@/components/ui/sidebar';

interface Dest {
  to: LinkProps['to'];
  label: string;
  icon: typeof Boxes;
  adminOnly: boolean;
  match: (p: string) => boolean;
}

const DESTS: Dest[] = [
  { to: '/sessions', label: 'Sessions', icon: Layers, adminOnly: false, match: (p) => p === '/' || p.startsWith('/sessions') },
  { to: '/fleet', label: 'Fleet', icon: Boxes, adminOnly: true, match: (p) => p.startsWith('/fleet') },
  { to: '/storage', label: 'Storage', icon: HardDrive, adminOnly: true, match: (p) => p.startsWith('/storage') },
  { to: '/settings', label: 'Settings', icon: Settings, adminOnly: false, match: (p) => p.startsWith('/settings') },
];

export function MainSidebar() {
  const isAdmin = useIsAdmin();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const dests = DESTS.filter((d) => !d.adminOnly || isAdmin);

  return (
    <Sidebar collapsible="icon" className="border-r-sidebar">
      <div
        aria-hidden
        className="bg-carbon-fade pointer-events-none absolute inset-0 -z-10"
      />
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton asChild size="lg">
              <Link to="/sessions" aria-label="engrams — sessions">
                <span className="flex aspect-square size-8 items-center justify-center">
                  <EngramMark size={26} mode="static" />
                </span>
                <span className="font-semibold">engrams</span>
              </Link>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>

      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupContent>
            <SidebarMenu>
              {dests.map((d) => (
                <SidebarMenuItem key={d.label}>
                  <SidebarMenuButton asChild isActive={d.match(pathname)} tooltip={d.label}>
                    <Link to={d.to}>
                      <d.icon />
                      <span>{d.label}</span>
                    </Link>
                  </SidebarMenuButton>
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>

      <SidebarFooter className="bg-sidebar">
        <div className="flex items-center justify-between gap-2 px-1 group-data-[collapsible=icon]:flex-col">
          <ModeToggle />
          <SidebarTrigger className="text-sidebar-foreground hover:bg-sidebar-accent hover:text-sidebar-accent-foreground" />
        </div>
        <UserMenu />
      </SidebarFooter>
    </Sidebar>
  );
}
