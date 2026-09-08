import { Outlet, useRouterState } from "@tanstack/react-router";
import { Plus, Workflow } from "lucide-react";

import { SectionLayout, SectionPage, type SectionNavItem } from "@/components/section-layout";
import { AutomationsRail } from "./AutomationsRail";

// The /automations section: a spine product, not a settings page. The rail is a
// persistent switcher over automations (AutomationsRail); the list, the
// composer, the workstreams and the activity ledger render into a padded,
// scrolling sheet. The Builder is the exception: it is a fixed-height frame
// whose canvas pans, so the editor routes get the bare sheet.
// The whole section is admin-gated at the route layer (see router.tsx).

const SECTION_PAGES = new Set(["new", "workstreams", "activity"]);

/** `/automations/$id` and `/automations/new/manual` are the Builder. */
export function isBuilderPath(pathname: string): boolean {
  const rest = pathname.replace(/^\/automations\/?/, "");
  if (rest === "") return false;
  if (rest === "new/manual") return true;
  const [head] = rest.split("/");
  return !SECTION_PAGES.has(head ?? "");
}

export function AutomationsLayout() {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const nav: SectionNavItem[] = [
    {
      to: "/automations",
      label: "Automations",
      icon: Workflow,
      active: pathname === "/automations" || pathname === "/automations/",
    },
    {
      to: "/automations/new",
      label: "New automation",
      icon: Plus,
      active: pathname.startsWith("/automations/new"),
    },
  ];
  return (
    <SectionLayout rail={<AutomationsRail />} railLabel="Automations" nav={nav}>
      {isBuilderPath(pathname) ? (
        <div className="flex min-h-0 flex-1 flex-col">
          <Outlet />
        </div>
      ) : (
        <SectionPage>
          <Outlet />
        </SectionPage>
      )}
    </SectionLayout>
  );
}
