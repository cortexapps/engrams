/** `/settings/reviewed-repos` → the PR-review built-in's Inputs tab (phase
 * 3.8). The built-in is resolved by its stable key, never a hardcoded id.
 * Until a deploy has seeded it, the page explains instead of bouncing to a
 * broken route. */

import { Link, Navigate } from "@tanstack/react-router";

import { SkeletonRows } from "@/components/skeleton-rows";
import { useBuiltinAutomation } from "@/hooks/useAutomations";

export const PR_REVIEW_BUILTIN_KEY = "pr_review";

export interface RedirectToBuiltinProps {
  builtinKey?: string;
}

export function RedirectToBuiltin({ builtinKey = PR_REVIEW_BUILTIN_KEY }: RedirectToBuiltinProps) {
  const query = useBuiltinAutomation(builtinKey);
  if (query.isPending) {
    return <SkeletonRows />;
  }
  const automation = query.data?.automation;
  if (automation) {
    return (
      <Navigate
        to="/automations/$id"
        params={{ id: automation.id }}
        search={{ tab: "inputs" }}
        replace
      />
    );
  }
  return (
    <div className="flex max-w-xl flex-col gap-2" aria-label="reviewed repos moved">
      <h2 className="text-base font-semibold">Reviewed repos moved</h2>
      <p className="text-muted-foreground text-sm">
        Which repositories get reviews is now an input on the built-in “PR review” automation. This
        deployment has not seeded that automation yet; once it has, this page opens its Inputs tab.
      </p>
      <Link to="/automations" className="text-sm underline">
        Open Automations
      </Link>
    </div>
  );
}
