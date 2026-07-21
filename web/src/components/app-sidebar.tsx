import { Bandage, ScanSearch, Server, SquareTerminal } from "lucide-react";
import { Link, useRouterState } from "@tanstack/react-router";
import { useIsAdmin } from "../auth/AuthProvider";
import { useOperatorHealth } from "../hooks/useOperatorHealth";
import type { NavItem } from "./nav";
import { EngramMark } from "./EngramMark";
import { ModeToggle } from "./mode-toggle";
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
  SidebarTrigger,
} from "@/components/ui/sidebar";

interface Dest extends NavItem {
  adminOnly: boolean;
  match: (p: string) => boolean;
}

// The top-level hats: Sessions is the developer surface; Reviews is the shared
// PR review ledger; Kaizen gathers the small frictions agents report while
// working; Operator gathers the whole admin/infrastructure surface (fleet,
// storage, images, registries) behind its own rail. Account + org config
// (Settings) lives in the avatar menu at the foot of the rail, the one canonical
// entry point.
//
// Each hat's glyph is deliberately distinct from its section's landing item
// (Sessions → "My sessions" = Layers; Kaizen → "Papercuts" = ListChecks;
// Operator → "Overview" = Gauge), so the rail and the open section sidebar
// never show the same icon twice in adjacent columns.
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
    icon: ScanSearch,
    adminOnly: false,
    match: (p) => p.startsWith("/reviews"),
  },
  {
    to: "/kaizen",
    label: "Kaizen",
    icon: Bandage,
    adminOnly: false,
    match: (p) => p.startsWith("/kaizen"),
  },
  {
    to: "/operator",
    label: "Operator",
    icon: Server,
    adminOnly: true,
    match: (p) => p.startsWith("/operator"),
  },
];

export function MainSidebar() {
  const isAdmin = useIsAdmin();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const dests = DESTS.filter((d) => !d.adminOnly || isAdmin);

  // border-r-sidebar is load-bearing: it recolours the Sidebar's default right
  // border to the spine's own fill, suppressing the divider line that would
  // otherwise sit between this rail and the section sidebar.
  return (
    <Sidebar collapsible="icon" className="border-r-sidebar">
      <div aria-hidden className="bg-carbon-fade pointer-events-none absolute inset-0 -z-10" />
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton asChild size="lg">
              <Link to="/sessions" aria-label="engrams — tasks">
                <span
                  className="flex aspect-square size-8 items-center justify-center"
                  style={{ color: "var(--sidebar-primary)" }}
                >
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
                  {d.to === "/operator" && <OperatorRailSignal />}
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

// A quiet telltale on the Operator rail item: invisible when all-nominal, it
// lights amber (caution) or red (critical) the instant fleet or storage health
// slips — so an operator working in Sessions still catches a draining host or a
// stale flush window without parking on the cockpit. It rides the top-right
// corner of the item, so it survives the rail collapsing to icons (exactly when
// the label is gone and the signal matters most). Status colour only — never the
// lime accent — and the urgent ping is reserved for critical and respects
// reduced motion. The row still owns the click; the dot is pointer-transparent.
function OperatorRailSignal() {
  const { tone, reason } = useOperatorHealth();
  if (!tone) return null;
  const color = `var(--color-instrument-${tone})`;

  return (
    <span
      role="img"
      aria-label={`Operator needs attention: ${reason}`}
      className="pointer-events-none absolute right-1.5 top-1.5 z-10 flex size-2"
    >
      {tone === "critical" && (
        <span
          aria-hidden
          className="absolute inline-flex size-full animate-ping rounded-full opacity-60 [animation-duration:1.8s] motion-reduce:hidden"
          style={{ backgroundColor: color }}
        />
      )}
      <span
        className="relative inline-flex size-2 rounded-full"
        style={{ backgroundColor: color, boxShadow: `0 0 5px 0 ${color}` }}
      />
    </span>
  );
}
