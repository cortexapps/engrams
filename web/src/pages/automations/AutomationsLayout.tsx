import { Outlet, useRouterState } from "@tanstack/react-router";
import { Plus, Workflow } from "lucide-react";

import { SectionLayout, SectionPage, type SectionNavItem } from "@/components/section-layout";
import { AutomationsRail } from "./AutomationsRail";

// The /automations section: a spine product, not a settings page. The rail is a
// persistent switcher over automations (AutomationsRail); the list, the
// composer, and the editor render into the sheet. The whole section is
// admin-gated at the route layer (see router.tsx).
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
      <SectionPage>
        <Outlet />
      </SectionPage>
    </SectionLayout>
  );
}
