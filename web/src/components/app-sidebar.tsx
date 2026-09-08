import {
  FileBox,
  FilePenLine,
  GitPullRequestArrow,
  Settings,
  SquareTerminal,
  Workflow,
} from "lucide-react";
import { Link, useRouterState } from "@tanstack/react-router";
import { useIsAdmin } from "../auth/AuthProvider";
import type { NavItem } from "./nav";
import { EngramMark } from "./EngramMark";
import { UserMenu } from "./user-menu";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarRail,
  SidebarSeparator,
} from "@/components/ui/sidebar";
import { cn } from "@/lib/utils";

interface Dest extends NavItem {
  adminOnly: boolean;
  /**
   * Kept out of the spine for everyone, whatever their role. This is not
   * authorization — `adminOnly` is — it is "this is not ready to be met yet".
   * Deleting the line restores the destination as it was.
   */
  unreleased?: boolean;
  match: (p: string) => boolean;
}

// The spine holds PRODUCTS only: Tasks is the developer surface; Reviews is
// the shared PR review ledger; Artifacts the document library; Automations the
// event-driven work. Everything an admin configures — the account, the
// workspace, the runtime, the fleet — is one destination, Settings, at the
// foot of the spine as a normal row (not hidden in the avatar menu).
const DESTS: Dest[] = [
  {
    to: "/sessions",
    label: "Tasks",
    icon: SquareTerminal,
    adminOnly: false,
    match: (p) => p === "/" || p.startsWith("/sessions"),
  },
  {
    to: "/reviews",
    label: "Reviews",
    icon: GitPullRequestArrow,
    adminOnly: false,
    match: (p) => p.startsWith("/reviews"),
  },
  {
    to: "/artifacts",
    label: "Artifacts",
    icon: FileBox,
    adminOnly: false,
    match: (p) => p.startsWith("/artifacts"),
  },
  {
    to: "/specs",
    label: "Tech Specs",
    icon: FilePenLine,
    adminOnly: false,
    // Tech Specs is still being finished, so nobody meets it from the spine —
    // admins included. This hides the entry point, not the feature: /specs
    // still answers on a direct link and the orchestrator still serves every
    // spec RPC, so anyone holding a spec URL keeps their spec. Delete the line
    // below when the feature is ready.
    unreleased: true,
    match: (p) => p.startsWith("/specs"),
  },
  {
    to: "/automations",
    label: "Automations",
    icon: Workflow,
    adminOnly: true,
    match: (p) => p.startsWith("/automations"),
  },
];

const SETTINGS: Dest = {
  to: "/settings",
  label: "Settings",
  icon: Settings,
  adminOnly: false,
  match: (p) => p.startsWith("/settings"),
};

export function MainSidebar() {
  const isAdmin = useIsAdmin();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const dests = DESTS.filter((d) => !d.unreleased && (!d.adminOnly || isAdmin));

  // border-r-sidebar is load-bearing: it recolours the Sidebar's default right
  // border to the spine's own fill, suppressing the divider line that would
  // otherwise sit between this rail and the section rail.
  return (
    <Sidebar collapsible="icon" className="border-r-sidebar">
      <div aria-hidden className="bg-carbon-fade pointer-events-none absolute inset-0 -z-10" />
      <SidebarHeader className="px-[10px] pt-[14px] pb-[14px]">
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton
              asChild
              className="h-9 gap-2.5 px-2 hover:bg-transparent active:bg-transparent"
            >
              <Link to="/sessions" aria-label="engrams — tasks">
                <span
                  className="flex size-6 shrink-0 items-center justify-center"
                  style={{ color: "var(--sidebar-primary)" }}
                >
                  <EngramMark size={24} mode="static" />
                </span>
                <span className="text-base font-semibold tracking-[-0.01em]">engrams</span>
              </Link>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>

      <SidebarContent>
        <SidebarGroup className="px-[10px] py-0">
          <SidebarGroupContent>
            <SidebarMenu className="gap-0.5">
              {dests.map((d) => (
                <SpineRow key={d.label} dest={d} active={d.match(pathname)} />
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>

      {/* Settings is a destination row like the others; the hairline below it
          separates navigation from identity (the avatar row). */}
      <SidebarFooter className="bg-sidebar gap-1.5 px-[10px] pb-3">
        <SidebarMenu>
          <SpineRow dest={SETTINGS} active={SETTINGS.match(pathname)} />
        </SidebarMenu>
        <SidebarSeparator className="mx-0" />
        <UserMenu />
      </SidebarFooter>
      <SidebarRail />
    </Sidebar>
  );
}

/** One spine destination. Active: the raised wash, a heavier weight, and the
 * 3×16 lime bar at the spine's edge — the one place lime marks a place rather
 * than an action. The bar is a sibling of the button (the button clips its own
 * overflow), so it can sit in the 10px spine padding. */
function SpineRow({ dest, active }: { dest: Dest; active: boolean }) {
  return (
    <SidebarMenuItem>
      <SidebarMenuButton
        asChild
        isActive={active}
        tooltip={dest.label}
        className={cn(
          "h-[34px] gap-2.5 rounded-[10px] px-2.5 text-sm font-medium text-sidebar-foreground/[0.82]",
          "data-[active=true]:font-semibold data-[active=true]:text-sidebar-accent-foreground",
        )}
      >
        <Link to={dest.to}>
          <dest.icon />
          <span>{dest.label}</span>
        </Link>
      </SidebarMenuButton>
      {active && (
        <span
          aria-hidden
          className="pointer-events-none absolute top-[9px] -left-[10px] h-4 w-[3px] rounded-[2px] bg-sidebar-primary"
        />
      )}
    </SidebarMenuItem>
  );
}
