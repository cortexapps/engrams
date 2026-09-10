import { Outlet } from "@tanstack/react-router";

import { SectionLayout, SectionPage } from "@/components/section-layout";
import { SettingsRail, useSettingsNav } from "./SettingsRail";

// The /settings section: one nav rail over everything a person or an admin
// configures (see SettingsRail for the groups), and the page as a padded,
// scrolling sheet.
export function SettingsLayout() {
  const nav = useSettingsNav();
  return (
    <SectionLayout rail={<SettingsRail />} railLabel="Settings" nav={nav}>
      <SectionPage>
        <Outlet />
      </SectionPage>
    </SectionLayout>
  );
}
