import { Outlet } from "@tanstack/react-router";

import { SlackRail } from "./SlackRail";
import { SectionLayout } from "@/components/section-layout";

// The /slack section shell: the rail is the open-threads switcher, the page
// is the product's settings and its threads ledger — the Reviews shape.
export function SlackLayout() {
  return (
    <SectionLayout rail={<SlackRail />} railLabel="Slack threads">
      <Outlet />
    </SectionLayout>
  );
}
