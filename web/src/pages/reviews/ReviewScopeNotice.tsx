import { Link } from "@tanstack/react-router";

import type { ReviewScopeState } from "./review-scope";

/** Why "Mine" shows no pull requests, and where to fix it. */
export function ReviewScopeNotice({
  reason,
}: {
  reason: NonNullable<ReviewScopeState["unavailable"]>;
}) {
  const settings = (
    <Link to="/settings/credentials" className="underline underline-offset-2">
      Settings → Credentials
    </Link>
  );
  return reason === "not_connected" ? (
    <>To see your pull requests, connect your GitHub account in {settings}.</>
  ) : (
    // A GitHub token saved before engrams asked GitHub for the login names no
    // account; saving it again records the login.
    <>To see your pull requests, save your GitHub token again in {settings}.</>
  );
}
