import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import { Plus } from "lucide-react";

import { PageHeading } from "@/components/page-heading";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

const tabClass =
  "inline-flex h-10 items-center border-b-2 px-1 text-sm font-medium transition-colors";

export function SpecsLayout() {
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  return (
    <div className="section-sheet flex min-h-0 flex-1 flex-col overflow-hidden">
      <div className="shrink-0 px-4 pt-4 md:px-6 md:pt-6">
        <PageHeading
          title="Tech Specs"
          actions={
            <Button disabled title="The spec creation flow is not available yet">
              <Plus aria-hidden />
              New spec
            </Button>
          }
        />
        <nav aria-label="Tech Specs" className="mt-5 flex gap-6 border-b">
          <Link
            to="/specs"
            className={cn(
              tabClass,
              pathname === "/specs" ||
                (pathname.startsWith("/specs/") && !pathname.startsWith("/specs/templates"))
                ? "border-primary text-foreground"
                : "border-transparent text-muted-foreground hover:text-foreground",
            )}
          >
            Specs
          </Link>
          <Link
            to="/specs/templates"
            className={cn(
              tabClass,
              pathname.startsWith("/specs/templates")
                ? "border-primary text-foreground"
                : "border-transparent text-muted-foreground hover:text-foreground",
            )}
          >
            Templates
          </Link>
        </nav>
      </div>
      <Outlet />
    </div>
  );
}
