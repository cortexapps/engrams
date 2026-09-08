import type { CSSProperties, ReactNode } from "react";
import { Link, useRouterState, type LinkProps } from "@tanstack/react-router";

import { useIsAdmin } from "../../auth/AuthProvider";
import { useOperatorHealth } from "../../hooks/useOperatorHealth";
import { usePapercuts } from "../../hooks/usePapercuts";
import type { SectionNavItem } from "@/components/section-layout";
import {
  SidebarContent,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
} from "@/components/ui/sidebar";
import { cn } from "@/lib/utils";

// The Settings rail: everything a person or an admin configures, in five flat
// groups with no bundling — no "Operator" hat, no "Org" umbrella. A member sees
// their own pair plus the feedback they can read; an admin sees the workspace,
// the runtime it runs on, and the infrastructure underneath.

export interface SettingsItem {
  to: LinkProps["to"];
  label: string;
  /** Members can reach it (everything else is admin-only). */
  member?: boolean;
  /** A live readout on the trailing edge (Fleet health, Papercuts count). */
  trailing?: () => ReactNode;
}

export interface SettingsGroup {
  label: string;
  items: SettingsItem[];
}

export const SETTINGS_GROUPS: SettingsGroup[] = [
  {
    label: "You",
    items: [
      { to: "/settings/profile", label: "Profile", member: true },
      { to: "/settings/credentials", label: "Credentials", member: true },
    ],
  },
  {
    label: "Workspace",
    items: [
      { to: "/settings/members", label: "Members" },
      { to: "/settings/integrations", label: "Integrations" },
      { to: "/settings/secrets", label: "Secrets" },
      { to: "/settings/api-keys", label: "API keys" },
    ],
  },
  {
    label: "Runtime",
    items: [
      { to: "/settings/profiles", label: "Session profiles" },
      { to: "/settings/harnesses", label: "Harnesses" },
      { to: "/settings/model-routers", label: "Model routers" },
      { to: "/settings/images", label: "Images" },
      { to: "/settings/registries", label: "Registries" },
    ],
  },
  {
    label: "Infrastructure",
    items: [
      { to: "/settings/fleet", label: "Fleet", trailing: () => <FleetStatus /> },
      { to: "/settings/storage", label: "Storage" },
    ],
  },
  {
    label: "Feedback",
    items: [
      // Papercuts are agent feedback about tasks; members could always read
      // them, and moving the page did not change who it is for.
      {
        to: "/settings/papercuts",
        label: "Papercuts",
        member: true,
        trailing: () => <PapercutsCount />,
      },
    ],
  },
];

/** `/settings/profile` must not light `/settings/profiles`: match the segment,
 * not the prefix. */
export function isSettingsItemActive(pathname: string, to: string): boolean {
  return pathname === to || pathname.startsWith(`${to}/`);
}

/** The groups a role can see, with the empty ones dropped. */
export function visibleSettingsGroups(isAdmin: boolean): SettingsGroup[] {
  if (isAdmin) return SETTINGS_GROUPS;
  return SETTINGS_GROUPS.map((g) => ({ ...g, items: g.items.filter((it) => it.member) })).filter(
    (g) => g.items.length > 0,
  );
}

export function useSettingsNav(): SectionNavItem[] {
  const isAdmin = useIsAdmin();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  return visibleSettingsGroups(isAdmin).flatMap((g) =>
    g.items.map((it) => ({
      to: it.to,
      label: it.label,
      active: isSettingsItemActive(pathname, it.to as string),
    })),
  );
}

export function SettingsRail() {
  const isAdmin = useIsAdmin();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const groups = visibleSettingsGroups(isAdmin);
  // Row position across every group, for the boot stagger.
  let row = 0;

  return (
    <SidebarContent className="px-[10px] pt-[14px] pb-3">
      {groups.map((group, gi) => (
        <SidebarGroup key={group.label} className={cn("p-0", gi > 0 && "mt-3")}>
          {/* Louder than its rows: full ink, semibold — a heading quieter than
              its contents divides nothing. */}
          <SidebarGroupLabel className="h-auto px-2.5 pb-1.5 text-xs font-semibold text-sidebar-foreground">
            {group.label}
          </SidebarGroupLabel>
          <SidebarGroupContent>
            <SidebarMenu className="gap-0.5">
              {group.items.map((it) => (
                <SidebarMenuItem key={it.label} style={{ "--i": row++ } as CSSProperties}>
                  <SidebarMenuButton
                    asChild
                    isActive={isSettingsItemActive(pathname, it.to as string)}
                    className="rounded-[10px] px-2.5 text-sidebar-foreground/[0.84] data-[active=true]:font-semibold"
                  >
                    <Link to={it.to}>
                      <span className="min-w-0 flex-1 truncate">{it.label}</span>
                      {it.trailing?.()}
                    </Link>
                  </SidebarMenuButton>
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      ))}
    </SidebarContent>
  );
}

// The fleet's single worst issue rides the Fleet row, so an admin working in
// Tasks still catches a draining host without opening the page. Quiet when
// all-nominal; status colour only, never lime. Lives in its own component so
// the hook (two admin RPCs on a 30s poll) mounts only when the row does.
function FleetStatus() {
  const { tone, reason } = useOperatorHealth();
  if (!tone) return null;
  return (
    <span className="ml-auto inline-flex min-w-0 shrink items-center gap-1.5 font-mono text-2xs tabular-nums">
      <span
        aria-hidden
        className="size-1.5 shrink-0 rounded-full"
        style={{ backgroundColor: `var(--color-instrument-${tone})` }}
      />
      <span className="truncate">{reason}</span>
    </span>
  );
}

function PapercutsCount() {
  const { data } = usePapercuts();
  const n = data?.papercuts.length ?? 0;
  if (n === 0) return null;
  return (
    <span className="ml-auto font-mono text-2xs tabular-nums text-sidebar-foreground/70">{n}</span>
  );
}
