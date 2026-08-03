import { useMemo, useState } from "react";
import { Link, useRouterState } from "@tanstack/react-router";
import { TriangleAlert } from "lucide-react";

import { useNow } from "../../hooks/useNow";
import { useArtifacts } from "../../hooks/useArtifacts";
import { relativeTime } from "../sessions/session-format";
import { KIND_GLYPHS, mediaKind } from "../../lib/artifacts";
import {
  SidebarContent,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarInput,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarMenuSkeleton,
} from "@/components/ui/sidebar";

// The persistent artifacts rail: the user's documents, newest first
// (the default scope — mine for a member, everything for an admin).
// Each row is one artifact carrying its kind glyph and recency; it stays
// mounted across the library and the detail page.
export function ArtifactsRail() {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const { data, isPending, error } = useArtifacts("");
  const now = useNow();
  const [search, setSearch] = useState("");

  const rows = useMemo(() => {
    const all = data?.artifacts ?? [];
    const needle = search.trim().toLowerCase();
    if (!needle) return all;
    return all.filter(
      (a) => a.title.toLowerCase().includes(needle) || a.fileName.toLowerCase().includes(needle),
    );
  }, [data?.artifacts, search]);

  const openId = pathname.startsWith("/artifacts/") ? pathname.split("/")[2] : undefined;

  return (
    <>
      <SidebarHeader className="gap-2 px-2 pt-3">
        <SidebarInput
          value={search}
          onChange={(event) => setSearch(event.target.value)}
          placeholder="Search artifacts…"
          aria-label="Search artifacts"
          className="border-sidebar-border bg-sidebar-accent/40 text-sidebar-foreground placeholder:text-sidebar-foreground/80 dark:bg-sidebar-accent/40"
        />
      </SidebarHeader>

      <SidebarContent>
        <SidebarGroup className="py-1">
          <SidebarGroupLabel>Artifacts</SidebarGroupLabel>
          <SidebarGroupContent>
            <SidebarMenu>
              {isPending ? (
                Array.from({ length: 5 }).map((_, i) => (
                  <SidebarMenuItem key={i}>
                    <SidebarMenuSkeleton />
                  </SidebarMenuItem>
                ))
              ) : error ? (
                <p className="flex items-center gap-1.5 px-2 py-1.5 text-xs text-sidebar-foreground">
                  <TriangleAlert className="size-3.5 shrink-0" />
                  artifacts unavailable
                </p>
              ) : rows.length === 0 ? (
                <p className="px-2 py-1.5 text-xs text-sidebar-foreground/80">
                  {search ? "No matches." : "No artifacts yet."}
                </p>
              ) : (
                rows.map((a) => {
                  const Glyph = KIND_GLYPHS[mediaKind(a.mediaType)];
                  return (
                    <SidebarMenuItem key={a.id}>
                      <SidebarMenuButton asChild isActive={a.id === openId}>
                        <Link to="/artifacts/$artifactId" params={{ artifactId: a.id }}>
                          <Glyph className="size-4 shrink-0" />
                          <span className="min-w-0 flex-1 truncate">{a.title}</span>
                          <span className="shrink-0 font-mono text-[0.65rem] tabular-nums opacity-70">
                            {relativeTime(a.updatedAt, now)}
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
