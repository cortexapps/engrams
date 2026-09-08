import { Outlet } from "@tanstack/react-router";

import { ReviewsRail } from "./ReviewsRail";
import { SectionLayout } from "@/components/section-layout";

// The /reviews section shell. Its rail is the persistent ReviewsRail (a switcher
// over reviewed PRs), which stays mounted across the ledger AND the dossier —
// the dossier is a child route, so opening a PR moves the rail's highlight
// instead of swapping the whole layout.
//
// The outlet is layout-neutral: the ledger pads and scrolls itself; the dossier
// fills the height and manages its own panes. No mobile strip: on a phone the
// rail would crowd out the thing you came to read, and the ledger already lists
// every PR.
export function ReviewsLayout() {
  return (
    <SectionLayout rail={<ReviewsRail />} railLabel="Reviewed pull requests">
      <Outlet />
    </SectionLayout>
  );
}
