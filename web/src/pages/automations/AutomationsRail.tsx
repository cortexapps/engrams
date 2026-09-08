import { Plus } from "lucide-react";
import { Link, useRouterState } from "@tanstack/react-router";

import type { AutomationSummary } from "@/gen/engram/app/v1/automation_pb";
import { useAutomations } from "@/hooks/useAutomations";
import { useNow } from "@/hooks/useNow";
import { runStatusTone } from "@/lib/automations";
import { relativeAge } from "@/lib/relative-time";
import { orderAutomations } from "./AutomationsList";
import {
  SidebarContent,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarMenuSkeleton,
} from "@/components/ui/sidebar";
import { cn } from "@/lib/utils";

// The Automations rail: a switcher over every automation, the same shape as the
// Tasks rail — a status dot, the name, and a terse age — so opening one moves
// the highlight instead of swapping the layout. The Workstreams and Activity
// rows join it with their pages.

export function AutomationsRail() {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const now = useNow();
  const { data, isPending, error } = useAutomations();
  const rows = orderAutomations(data?.automations ?? []);
  const onNew = pathname.startsWith("/automations/new");

  return (
    <>
      <SidebarHeader className="px-[10px] pt-[14px]">
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton
              asChild
              isActive={onNew}
              className="h-[34px] rounded-[10px] px-2.5 text-sidebar-foreground/[0.84] data-[active=true]:font-semibold"
            >
              <Link to="/automations/new">
                <Plus />
                <span>New automation</span>
              </Link>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>

      <SidebarContent className="px-[10px] pb-3">
        <SidebarGroup className="p-0">
          <SidebarGroupLabel className="h-auto justify-between px-2.5 pt-2 pb-1.5 text-xs font-semibold text-sidebar-foreground">
            Automations
            {rows.length > 0 && (
              <span className="font-mono text-2xs font-normal tabular-nums text-sidebar-foreground/70">
                {rows.length}
              </span>
            )}
          </SidebarGroupLabel>
          <SidebarGroupContent>
            {isPending ? (
              <SidebarMenu>
                {Array.from({ length: 4 }).map((_, i) => (
                  <SidebarMenuItem key={i}>
                    <SidebarMenuSkeleton />
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            ) : error ? (
              <p className="px-2.5 py-1.5 text-xs text-sidebar-foreground/70">
                Couldn’t load automations.
              </p>
            ) : rows.length === 0 ? (
              <p className="px-2.5 py-1.5 text-xs text-sidebar-foreground/70">
                No automations yet.
              </p>
            ) : (
              <SidebarMenu className="gap-0.5">
                {rows.map((summary) => (
                  <AutomationRow
                    key={summary.automation?.id}
                    summary={summary}
                    active={
                      !!summary.automation &&
                      pathname.startsWith(`/automations/${summary.automation.id}`)
                    }
                    now={now}
                  />
                ))}
              </SidebarMenu>
            )}
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>
    </>
  );
}

/** Paused reads as muted (the switch is off); otherwise the last run's tone —
 * critical when it failed, nominal when it did not, muted when it never ran. */
export function railTone(summary: AutomationSummary): "nominal" | "critical" | "muted" {
  if (!summary.automation?.enabled) return "muted";
  const status = summary.lastRun?.status;
  if (!status) return "muted";
  return runStatusTone(status) === "critical" ? "critical" : "nominal";
}

function AutomationRow({
  summary,
  active,
  now,
}: {
  summary: AutomationSummary;
  active: boolean;
  now: number;
}) {
  const automation = summary.automation;
  if (!automation) return null;
  const tone = railTone(summary);
  const paused = !automation.enabled;
  const at = summary.lastRun?.startedAt;

  return (
    <SidebarMenuItem>
      <SidebarMenuButton
        asChild
        isActive={active}
        className={cn(
          "rounded-[10px] px-2.5 text-sidebar-foreground/[0.84] data-[active=true]:font-semibold",
          paused && "opacity-70",
        )}
      >
        <Link to="/automations/$id" params={{ id: automation.id }} search={{ tab: "build" }}>
          <span
            aria-hidden
            className="w-3.5 shrink-0 text-center text-2xs leading-none"
            style={{
              color: tone === "muted" ? undefined : `var(--color-instrument-${tone})`,
              opacity: tone === "muted" ? 0.55 : 1,
            }}
          >
            {tone === "muted" ? "○" : "●"}
          </span>
          <span className="min-w-0 flex-1 truncate">{automation.name}</span>
          <span className="ml-auto shrink-0 font-mono text-2xs tabular-nums text-sidebar-foreground/[0.65]">
            {paused ? "paused" : at ? relativeAge(at, now) : "—"}
          </span>
        </Link>
      </SidebarMenuButton>
    </SidebarMenuItem>
  );
}
