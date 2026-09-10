import { Badge } from "@/components/ui/badge";
import { StatusDot } from "@/components/status-dot";

export type TicketSyncState = "none" | "pending" | "synced" | "failed";

export function SpecTicketSyncBadge({ state }: { state: string }) {
  if (state === "none" || state === "") return null;
  if (state === "synced") {
    return (
      <Badge variant="outline" data-state="synced">
        <StatusDot tone="nominal" size={6} />
        Synced
      </Badge>
    );
  }
  if (state === "failed") {
    return (
      <Badge variant="outline" data-state="failed">
        <StatusDot tone="critical" size={6} />
        Sync failed
      </Badge>
    );
  }
  return (
    <Badge variant="outline" data-state="pending">
      <StatusDot tone="active" size={6} />
      Syncing
    </Badge>
  );
}
