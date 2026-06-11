import { useHosts } from "../hooks/useHosts";
import { useSessions } from "../hooks/useSessions";
import { useDrainHost } from "../hooks/useDrainHost";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { PageHeading } from "../components/page-heading";
import { StatReadout } from "../components/stat-readout";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Progress } from "@/components/ui/progress";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { cn } from "@/lib/utils";
import type { HostStatus, HostView, Session } from "../types";

const statusVariant = (s: HostStatus) =>
  s === "ready" ? "default" : s === "draining" ? "secondary" : "destructive";

// One labelled utilization meter (disk / mem / cpu). For disk + mem
// pass `usedMib`/`totalMib` and the readout shows GiB; for cpu pass
// `pct` directly. The fill turns destructive past 80% — the "this host
// is about to fall over" threshold an operator scans for.
function Meter({
  label,
  usedMib,
  totalMib,
  pct,
}: {
  label: string;
  usedMib?: number;
  totalMib?: number;
  pct?: number;
}) {
  const ratio =
    pct !== undefined ? pct : totalMib && totalMib > 0 ? ((usedMib ?? 0) / totalMib) * 100 : 0;
  const shown = Math.min(100, Math.max(0, Math.round(ratio)));
  const hot = shown >= 80;
  const known = pct !== undefined || (totalMib ?? 0) > 0;
  const readout =
    pct !== undefined
      ? `${shown}%`
      : known
        ? `${((usedMib ?? 0) / 1024).toFixed(1)}/${((totalMib ?? 0) / 1024).toFixed(0)} GiB`
        : "—";
  return (
    <div className="flex items-center gap-3" title={`${label}: ${shown}%`}>
      <span className="w-10 shrink-0 font-mono text-[0.66rem] uppercase tracking-wide text-muted-foreground">
        {label}
      </span>
      <Progress value={shown} className="h-2" indicatorClassName={cn(hot && "bg-destructive")} />
      <span className="w-24 shrink-0 text-right font-mono text-xs tabular-nums text-muted-foreground">
        {readout}
      </span>
    </div>
  );
}

export function Fleet() {
  const { data: hosts } = useHosts();
  const { data: sessions } = useSessions();
  const drain = useDrainHost();
  const h = hosts ?? [];
  const s = sessions ?? [];
  const totalSb = h.reduce((a, x) => a + x.running_sandboxes, 0);
  // Fleet-wide disk is the headline operational number — a full host
  // disk is what bricks sessions. (Memory + CPU live per-host.)
  const usedGiB = (h.reduce((a, x) => a + x.util_disk_used_mib, 0) / 1024).toFixed(0);
  const totGiB = (h.reduce((a, x) => a + x.util_disk_total_mib, 0) / 1024).toFixed(0);
  const anyDraining = h.some((x) => x.status === "draining");
  const liveByHost = (id: string) =>
    s.filter((x: Session) => x.host_id === id && x.status === "active").length;

  return (
    <div className="space-y-6">
      <PageHeading title="Fleet" description="Firecracker hosts and capacity." />

      <StatReadout
        className="sm:grid-cols-3"
        items={[
          { label: "Hosts", value: h.length },
          { label: "Sandboxes", value: totalSb },
          { label: "GiB disk", value: `${usedGiB}/${totGiB}` },
        ]}
      />

      {h.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          No hosts have registered yet. Hosts appear here once they boot and complete their first
          heartbeat.
        </p>
      ) : (
        <div className="space-y-3">
          {h.map((host) => (
            <HostCard
              key={host.id}
              host={host}
              live={liveByHost(host.id)}
              onDrain={() => drain.mutate(host.id)}
              draining={drain.isPending}
            />
          ))}
        </div>
      )}

      <p className="text-sm text-muted-foreground">
        Reconciler{" "}
        {anyDraining
          ? "rebalancing — draining host, migrating sandboxes"
          : "steady — desired state matches observed"}
        .
      </p>
    </div>
  );
}

function HostCard({
  host,
  live,
  onDrain,
  draining,
}: {
  host: HostView;
  live: number;
  onDrain: () => void;
  draining: boolean;
}) {
  const diskUsedGiB = (host.util_disk_used_mib / 1024).toFixed(1);
  const diskTotalGiB = (host.util_disk_total_mib / 1024).toFixed(0);
  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between gap-3 space-y-0">
        <CardTitle className="font-mono text-base">{host.id}</CardTitle>
        <div className="flex items-center gap-3">
          <Badge variant={statusVariant(host.status)}>{host.status}</Badge>
          {host.status === "ready" && (
            <AlertDialog>
              <AlertDialogTrigger asChild>
                <Button variant="outline" size="sm" disabled={draining}>
                  Drain
                </Button>
              </AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>Drain {host.id}?</AlertDialogTitle>
                  <AlertDialogDescription>
                    The scheduler stops assigning new sessions to this host. In-flight sessions stay
                    put.
                  </AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel>Cancel</AlertDialogCancel>
                  <AlertDialogAction onClick={onDrain}>Drain</AlertDialogAction>
                </AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          )}
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        {host.running_sandboxes === 0 ? (
          <p className="text-sm italic text-muted-foreground">no sandboxes</p>
        ) : (
          <div className="flex flex-wrap gap-1" aria-label={`${host.running_sandboxes} sandboxes`}>
            {Array.from({ length: host.running_sandboxes }, (_, i) => (
              <span
                key={i}
                className={`size-3 ${i < live ? "bg-primary" : "bg-muted-foreground/40"}`}
              />
            ))}
          </div>
        )}
        <div className="space-y-2">
          <Meter
            label="disk"
            usedMib={host.util_disk_used_mib}
            totalMib={host.util_disk_total_mib}
          />
          <Meter label="mem" usedMib={host.util_mem_used_mib} totalMib={host.util_mem_total_mib} />
          <Meter label="cpu" pct={host.util_cpu_pct} />
        </div>
        <div className="font-mono text-xs text-muted-foreground">
          {host.running_sandboxes} sandboxes
          {host.util_disk_total_mib > 0 && (
            <>
              {" "}
              · {diskUsedGiB}/{diskTotalGiB} GiB disk
            </>
          )}{" "}
          · {host.local_snapshots} snapshots
        </div>
      </CardContent>
    </Card>
  );
}
