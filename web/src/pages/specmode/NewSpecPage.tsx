import { useNavigate } from "@tanstack/react-router";

import { NewSpecSheet } from "@/pages/specs/NewSpecSheet";

/** F7 replaces this route with the dedicated creation screen. */
export function NewSpecPage() {
  const navigate = useNavigate();
  return (
    <main className="fixed inset-0 bg-background" aria-label="New spec">
      <NewSpecSheet
        open
        onOpenChange={(open) => {
          if (!open) void navigate({ to: "/specs" });
        }}
      />
    </main>
  );
}
