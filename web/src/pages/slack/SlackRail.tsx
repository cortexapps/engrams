import { Link } from "@tanstack/react-router";
import { MessageSquare, TriangleAlert } from "lucide-react";

import { useNow } from "../../hooks/useNow";
import { useBuiltinAutomation } from "../../hooks/useAutomations";
import { useInputKeyLabels } from "../../hooks/useAutomationInputs";
import { useInstanceList } from "../../hooks/useInstances";
import { relativeAge } from "@/lib/relative-time";
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
import { SLACK_BRAIN_BUILTIN_KEY } from "./SlackThreads";
import { channelNames, describeThread, threadChannelIds } from "./slack-format";

// The persistent Slack rail: a switcher over the OPEN threads engrams is in.
// Each row is one thread workstream and navigates to its workstream page.
export function SlackRail() {
  const now = useNow();
  const builtin = useBuiltinAutomation(SLACK_BRAIN_BUILTIN_KEY);
  const automationId = builtin.data?.automation?.id;
  const list = useInstanceList(automationId, { includeClosed: false });
  const threads = list.data?.instances ?? [];
  const names = channelNames(
    useInputKeyLabels("channel", threadChannelIds(threads.map((t) => t.key))).data?.options,
  );

  return (
    <>
      <SidebarHeader className="gap-1 px-3 pt-3">
        <span className="flex items-center gap-1.5 text-sm font-medium">
          <MessageSquare className="size-3.5" aria-hidden />
          Slack
        </span>
      </SidebarHeader>
      <SidebarContent>
        <SidebarGroup className="py-1">
          <SidebarGroupLabel>Open threads</SidebarGroupLabel>
          <SidebarGroupContent>
            <SidebarMenu>
              {builtin.isPending || (automationId && list.isPending) ? (
                Array.from({ length: 3 }).map((_, i) => (
                  <SidebarMenuItem key={i}>
                    <SidebarMenuSkeleton />
                  </SidebarMenuItem>
                ))
              ) : list.error ? (
                <p className="flex items-center gap-1.5 px-2 py-1.5 text-xs text-sidebar-foreground">
                  <TriangleAlert className="size-3.5 shrink-0" />
                  Couldn’t load threads.
                </p>
              ) : threads.length === 0 ? (
                <p className="px-2 py-2 text-xs text-sidebar-foreground/85">No open threads.</p>
              ) : (
                threads.map((thread) => {
                  const { title, subtitle } = describeThread(thread, names);
                  return (
                    <SidebarMenuItem key={thread.id}>
                      <SidebarMenuButton asChild className="h-auto items-start gap-2.5 py-1.5">
                        <Link
                          to="/automations/workstreams/$id"
                          params={{ id: thread.id }}
                          title={thread.key}
                        >
                          <span className="flex min-w-0 flex-1 flex-col">
                            <span className="truncate text-sm leading-tight">{title}</span>
                            <span className="truncate font-mono text-2xs leading-tight text-sidebar-foreground/85">
                              {subtitle}
                            </span>
                          </span>
                          <span
                            title={thread.openedAt}
                            className="mt-0.5 shrink-0 font-mono text-2xs tabular-nums text-sidebar-foreground/85"
                          >
                            {relativeAge(thread.openedAt, now)}
                          </span>
                        </Link>
                      </SidebarMenuButton>
                    </SidebarMenuItem>
                  );
                })
              )}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>
    </>
  );
}
