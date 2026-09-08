import { Layers, ListChecks, SquarePlus } from "lucide-react";
import { Outlet, useRouterState, type LinkProps } from "@tanstack/react-router";
import { useIsAdmin } from "../../auth/AuthProvider";
import { SectionLayout, type SectionNavItem } from "@/components/section-layout";
import { SessionsRail } from "./SessionsRail";

// The /sessions section shell. Its rail is the persistent SessionsRail (a live
// switcher), which stays mounted across the list views AND the transcript —
// SessionDetail is a child route, so opening a session moves the rail's
// highlight instead of swapping the whole layout. The outlet is layout-neutral:
// each child owns its padding/scroll (the list pages pad + scroll; the detail
// page fills the height with its own panes).
// A long task title needs more room than a nav rail does, so the rail is
// drag-resizable and remembers the width per browser.
const RAIL_WIDTH_STORAGE_KEY = "engrams.sessionsRailWidth";

export function SessionsLayout() {
  const isAdmin = useIsAdmin();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const seg = pathname.startsWith("/sessions/") ? pathname.split("/")[2] : undefined;
  // `list` and `all` are section pages (the full tables), not a transcript — the
  // mobile scope strip stays up on them and only hides inside a session detail.
  const onDetail = !!seg && seg !== "all" && seg !== "list";

  const scopes: SectionNavItem[] = [
    {
      to: "/sessions",
      label: "Start",
      icon: SquarePlus,
      active: pathname === "/sessions" || pathname === "/sessions/",
    },
    {
      to: "/sessions/list" as LinkProps["to"],
      label: "My tasks",
      icon: Layers,
      active: pathname.startsWith("/sessions/list"),
    },
    ...(isAdmin
      ? [
          {
            to: "/sessions/all" as LinkProps["to"],
            label: "All tasks",
            icon: ListChecks,
            active: pathname.startsWith("/sessions/all"),
          },
        ]
      : []),
  ];

  // The list views are one page, so they lift as one sheet. A transcript is two
  // working surfaces — the thread and the work pane — so it builds its own pair
  // of sheets and the container stays a plain region on the cover. The mobile
  // strip hides inside a transcript too, so it doesn't crowd the detail view.
  return (
    <SectionLayout
      rail={<SessionsRail />}
      railLabel="Tasks"
      resizeStorageKey={RAIL_WIDTH_STORAGE_KEY}
      resizeLabel="Resize task list"
      nav={onDetail ? undefined : scopes}
      sheet={!onDetail}
    >
      <Outlet />
    </SectionLayout>
  );
}
