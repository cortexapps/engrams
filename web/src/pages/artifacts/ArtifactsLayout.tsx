import { Outlet } from "@tanstack/react-router";

import { ArtifactsRail } from "./ArtifactsRail";
import { SectionLayout } from "@/components/section-layout";

// The /artifacts section shell — the Reviews/Tasks switcher-rail shape. The rail
// (recent artifacts) stays mounted across the library AND the detail page (a
// child route), so opening a document moves the highlight instead of swapping
// the layout.
//
// The outlet is layout-neutral: the library pads and scrolls itself; the detail
// page binds to the section height so an HTML artifact's iframe can fill it.
export function ArtifactsLayout() {
  return (
    <SectionLayout rail={<ArtifactsRail />} railLabel="Artifacts">
      <Outlet />
    </SectionLayout>
  );
}
