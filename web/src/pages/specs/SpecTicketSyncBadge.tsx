import { CircleAlert, CircleCheck, Clock3 } from "lucide-react";

import { Badge } from "@/components/ui/badge";

export type TicketSyncState = "none" | "pending" | "synced" | "failed";

export function SpecTicketSyncBadge({ state }: { state: string }) {
  if (state === "none" || state === "") return null;
  if (state === "synced") {
    return (
      <Badge
        variant="outline"
        data-state="synced"
        className="border-instrument-nominal/35 text-instrument-nominal-ink"
      >
        <CircleCheck aria-hidden />
        Synced
      </Badge>
    );
  }
  if (state === "failed") {
    return (
      <Badge variant="destructive" data-state="failed">
        <CircleAlert aria-hidden />
        Sync failed
      </Badge>
    );
  }
  return (
    <Badge variant="outline" data-state="pending" className="text-muted-foreground">
      <Clock3 aria-hidden />
      Syncing
    </Badge>
  );
}
