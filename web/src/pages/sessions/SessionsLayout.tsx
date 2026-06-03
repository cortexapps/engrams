import { Layers, ListChecks } from 'lucide-react';
import { Link, Outlet, useRouterState, type LinkProps } from '@tanstack/react-router';
import { useIsAdmin } from '../../auth/AuthProvider';
import {
  Sidebar, SidebarContent, SidebarGroup, SidebarGroupContent, SidebarGroupLabel,
  SidebarMenu, SidebarMenuButton, SidebarMenuItem, SidebarProvider,
} from '@/components/ui/sidebar';
import { cn } from '@/lib/utils';

export function SessionsLayout() {
  const isAdmin = useIsAdmin();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const items: { to: LinkProps['to']; label: string; icon: typeof Layers; active: boolean }[] = [
    { to: '/sessions', label: 'My sessions', icon: Layers, active: pathname === '/sessions' || pathname === '/sessions/' },
    ...(isAdmin
      ? [{ to: '/sessions/all' as LinkProps['to'], label: 'All sessions', icon: ListChecks, active: pathname.startsWith('/sessions/all') }]
      : []),
  ];

  return (
    <SidebarProvider className="min-h-0 flex-1">
      {/* desktop (md+): vertical second sidebar */}
      <Sidebar collapsible="none" className="hidden border-r md:flex">
        <SidebarContent>
          <SidebarGroup>
            <SidebarGroupLabel>Sessions</SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu>
                {items.map((it) => (
                  <SidebarMenuItem key={it.label}>
                    <SidebarMenuButton asChild isActive={it.active}>
                      <Link to={it.to}><it.icon /><span>{it.label}</span></Link>
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        </SidebarContent>
      </Sidebar>
      <div className="flex flex-1 flex-col overflow-auto">
        {/* mobile (<md): horizontal nav strip */}
        <nav className="flex gap-1 overflow-x-auto border-b p-2 md:hidden">
          {items.map((it) => (
            <Link key={it.label} to={it.to}
              className={cn('inline-flex items-center gap-2 whitespace-nowrap rounded-md px-3 py-1.5 text-sm',
                it.active ? 'bg-accent text-accent-foreground' : 'text-muted-foreground')}>
              <it.icon className="size-4" />{it.label}
            </Link>
          ))}
        </nav>
        <div className="flex-1 p-4 md:p-6"><Outlet /></div>
      </div>
    </SidebarProvider>
  );
}
