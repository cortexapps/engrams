import { toast } from "sonner";

import { useHosts } from "../hooks/useHosts";
import { useStorageSummary } from "../hooks/useStorageSummary";
import { useDrainHost, useUndrainHost } from "../hooks/useDrainHost";
import { useNow } from "../hooks/useNow";
import {
  deriveHealthMetrics,
  operatorIssues,
  type HealthIssue,
  type HealthMetrics,
} from "../operator-health";
import { PageHeading } from "../components/page-heading";
import { EmptyState } from "@/components/empty-state";
import { localityTone, Meter } from "@/components/meter";
import { ReadoutCell, ReadoutStrip } from "@/components/readout-strip";
import { SkeletonRows } from "@/components/skeleton-rows";
import { StatusDot, type StatusTone } from "@/components/status-dot";
import { Button } from "@/components/ui/button";
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
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { errorMessage } from "../lib/errors";
import { relativeAge } from "@/lib/relative-time";
import { cn } from "@/lib/utils";
import type { HostStatus, HostView } from "../lib/types";

// Settings › Fleet: the one page for whoever runs the hosts. The health strip
// answers "is the fleet healthy?" in one read — a verdict with a sentence, the
// two continuous numbers (capacity, base locality), the resident sandbox count
// from the hosts' own heartbeats, and a telltale per fault, each with a count.
// Below it, one row per host with the figures an operator scans for and the
// one action they take here (drain / undrain).
//
// Every number comes from host truth (`listHosts`, `getStorageSummary`), never
// from the task list: that list is paged and scoped to the caller, so a count
// derived from it was a lower bound of one page of one person's tasks.

const STATUS_TONE: Record<HostStatus, StatusTone> = {
  ready: "nominal",
  draining: "caution",
  dead: "critical",
};

const HOST_COLUMNS = ["130px", "96px", "minmax(0, 1fr)", "140px", "140px", "88px", "70px"];

export interface FleetVerdict {
  tone: StatusTone;
  headline: string;
  /** One sentence that says what the headline means for the sandboxes. */
  sentence: string;
}

/** Up to two host ids by name, then a count: "host-04", "host-04 and host-05",
 * "host-04, host-05 and 2 more". */
export function hostNames(ids: string[]): string {
  if (ids.length <= 2) return ids.join(" and ");
  return `${ids[0]}, ${ids[1]} and ${ids.length - 2} more`;
}

function explain(issue: HealthIssue, hosts: HostView[]): string {
  switch (issue.kind) {
    case "offline": {
      const dead = hosts.filter((h) => h.status === "dead").map((h) => h.id);
      return `${hostNames(dead)} stopped heartbeating; its sandboxes resume elsewhere from their snapshots.`;
    }
    case "draining": {
      const draining = hosts.filter((h) => h.status === "draining").map((h) => h.id);
      return `Sandboxes on ${hostNames(draining)} finish in place; nothing new lands there.`;
    }
    case "capacity":
      return "New sandboxes queue until a host frees memory.";
    case "locality":
      return "Base chunks are coming from the cold tier, so boots run slower.";
    case "flush_window":
      return "Dirty chunks older than the flush window are at risk if their host is lost.";
    case "caps":
      return "Placement skips a host that fails a capability check, so free hosts may not take work.";
  }
}

/** The verdict cell's contents. The judgement itself lives in
 * operator-health.ts (shared with the Settings rail row); this only frames the
 * worst issue as a headline and one sentence, plus the two states that are not
 * an issue: no fleet, and all clear. */
export function fleetVerdict(hosts: HostView[], m: HealthMetrics): FleetVerdict {
  if (hosts.length === 0) {
    return {
      tone: "muted",
      headline: "No hosts registered",
      sentence: "A host appears here after its first heartbeat.",
    };
  }
  const issues = operatorIssues(m);
  const top = issues[0];
  if (!top) {
    return {
      tone: "nominal",
      headline: "All nominal",
      sentence: "Every host heartbeats and every sandbox flushed inside the window.",
    };
  }
  return {
    tone: top.tone,
    headline: `${top.tone === "critical" ? "Critical" : "Caution"} · ${top.text}`,
    sentence: explain(top, hosts),
  };
}

/** Sandboxes resident on the fleet: the hosts' own count, summed. */
export function residentSandboxes(hosts: HostView[]): number {
  return hosts.reduce((a, h) => a + h.running_sandboxes, 0);
}

const gib = (mib: number, digits = 0) => (mib / 1024).toFixed(digits);

export function Fleet() {
  const hostsQuery = useHosts();
  const { data: storage } = useStorageSummary();
  const now = useNow(1_000);
  const hosts = hostsQuery.data ?? [];
  const m = deriveHealthMetrics(hosts, storage);
  const verdict = fleetVerdict(hosts, m);
  const capUsed = hosts.reduce((a, h) => a + h.capacity_used_mib, 0);
  const capTotal = hosts.reduce((a, h) => a + h.capacity_total_mib, 0);
  const refreshed = hostsQuery.dataUpdatedAt
    ? relativeAge(new Date(hostsQuery.dataUpdatedAt).toISOString(), now)
    : null;

  const telltales: { label: string; tone: StatusTone; count: number }[] = [
    { label: "Draining", tone: "caution", count: m.draining },
    { label: "Offline", tone: "critical", count: m.dead },
    { label: "Past flush window", tone: "caution", count: m.rpoStale },
    { label: "Caps failing", tone: "caution", count: m.capsFailing },
  ];

  return (
    <div className="space-y-6">
      <PageHeading
        title="Fleet"
        count={hosts.length ? `${hosts.length} host${hosts.length === 1 ? "" : "s"}` : undefined}
        actions={
          refreshed && (
            <span className="font-mono text-xs tabular-nums text-muted-foreground">
              refreshed {refreshed} ago
            </span>
          )
        }
      />

      <ReadoutStrip columns="minmax(0, 1.3fr) repeat(4, minmax(0, 1fr))">
        <ReadoutCell
          label={
            <span className="inline-flex items-center gap-2 text-sm text-foreground">
              <StatusDot tone={verdict.tone} />
              {verdict.headline}
            </span>
          }
          tint={
            verdict.tone === "caution" || verdict.tone === "critical" ? verdict.tone : undefined
          }
        >
          <p className="text-xs leading-relaxed text-muted-foreground">{verdict.sentence}</p>
        </ReadoutCell>
        <ReadoutCell
          label="Capacity"
          value={`${m.capPct}%`}
          sub={capTotal > 0 ? `${gib(capUsed)}/${gib(capTotal)} GiB` : "—"}
        >
          <Meter value={capTotal > 0 ? m.capPct : null} label="capacity" />
        </ReadoutCell>
        {/* The parked/running split is not on the heartbeat yet; the hosts'
            own resident count is what we can state without guessing. */}
        <ReadoutCell
          label="Sandboxes resident"
          value={residentSandboxes(hosts)}
          sub={`across ${hosts.length} host${hosts.length === 1 ? "" : "s"}`}
        />
        <ReadoutCell
          label="Base locality"
          value={m.locality === null ? "—" : `${m.locality}%`}
          sub={storage ? `${storage.tracked_sandboxes} tracked` : undefined}
        >
          <Meter
            value={m.locality}
            tone={m.locality === null ? undefined : localityTone(m.locality)}
            label="base locality"
          />
        </ReadoutCell>
        <ReadoutCell label="Telltales">
          <ul className="flex flex-col gap-1.5">
            {telltales.map((t) => (
              <li key={t.label} className="flex items-center gap-2 text-xs">
                <StatusDot tone={t.count > 0 ? t.tone : "muted"} size={6} />
                <span className={t.count > 0 ? "text-foreground" : "text-muted-foreground"}>
                  {t.label}
                </span>
                <span
                  className={cn(
                    "ml-auto font-mono tabular-nums",
                    t.count > 0 ? "text-foreground" : "text-muted-foreground/60",
                  )}
                >
                  {t.count}
                </span>
              </li>
            ))}
          </ul>
        </ReadoutCell>
      </ReadoutStrip>

      {hostsQuery.isPending ? (
        <SkeletonRows rows={3} columns={HOST_COLUMNS} />
      ) : hostsQuery.error ? (
        <EmptyState tone="error">Couldn’t load hosts. {errorMessage(hostsQuery.error)}</EmptyState>
      ) : hosts.length === 0 ? (
        <EmptyState>No hosts registered. A host appears here after its first heartbeat.</EmptyState>
      ) : (
        <HostTable hosts={hosts} />
      )}
    </div>
  );
}

function HostTable({ hosts }: { hosts: HostView[] }) {
  const drain = useDrainHost();
  const undrain = useUndrainHost();
  const onUndrain = async (hostId: string) => {
    try {
      await undrain.mutateAsync({ hostId });
      toast.success(`${hostId} takes new sandboxes again`);
    } catch (error) {
      toast.error(errorMessage(error));
    }
  };
  return (
    <div className="rounded-lg border bg-card">
      <Table>
        <TableHeader>
          <TableRow className="hover:bg-transparent">
            <TableHead className="w-[130px]">Host</TableHead>
            <TableHead className="w-[96px]">Status</TableHead>
            <TableHead>Sandboxes</TableHead>
            <TableHead className="w-[140px]">Memory</TableHead>
            <TableHead className="w-[140px]">Disk</TableHead>
            <TableHead className="w-[88px]">CPU</TableHead>
            <TableHead className="w-[70px]" />
          </TableRow>
        </TableHeader>
        <TableBody>
          {hosts.map((host) => (
            <HostRow
              key={host.id}
              host={host}
              onDrain={() => drain.mutate({ hostId: host.id })}
              onUndrain={() => void onUndrain(host.id)}
              busy={drain.isPending || undrain.isPending}
            />
          ))}
        </TableBody>
      </Table>
    </div>
  );
}

/** How many sandbox cells to draw before the count takes over. */
const MAX_CELLS = 40;

function HostRow({
  host,
  onDrain,
  onUndrain,
  busy,
}: {
  host: HostView;
  onDrain: () => void;
  onUndrain: () => void;
  busy: boolean;
}) {
  const inFlight = [
    host.live_materializes > 0 ? `${host.live_materializes} materializing` : null,
    host.live_capture_jobs > 0 ? `${host.live_capture_jobs} capturing` : null,
  ].filter(Boolean);
  const caps =
    host.failing_capabilities.length > 0
      ? host.failing_capabilities.length === 1
        ? `${host.failing_capabilities[0]} failing`
        : `${host.failing_capabilities.length} caps failing`
      : host.capabilities_schema === 0
        ? "caps unreported"
        : null;
  const memPct =
    host.util_mem_total_mib > 0 ? (host.util_mem_used_mib / host.util_mem_total_mib) * 100 : null;
  const diskPct =
    host.util_disk_total_mib > 0
      ? (host.util_disk_used_mib / host.util_disk_total_mib) * 100
      : null;

  return (
    <TableRow
      data-status={host.status}
      className="hover:bg-transparent"
      style={
        host.status === "draining"
          ? {
              backgroundColor:
                "color-mix(in oklch, var(--color-instrument-caution) 6%, transparent)",
            }
          : undefined
      }
    >
      <TableCell className="align-top">
        <div className="font-mono text-xs">{host.id}</div>
        {host.fc_snapshot_version && (
          <div className="font-mono text-2xs text-muted-foreground">
            fc {host.fc_snapshot_version}
          </div>
        )}
      </TableCell>
      <TableCell className="align-top">
        <div className="inline-flex items-center gap-1.5 text-xs">
          <StatusDot tone={STATUS_TONE[host.status]} size={6} />
          {host.status}
        </div>
        {caps && (
          <div
            className="inline-flex items-center gap-1.5 font-mono text-2xs text-muted-foreground"
            title={
              host.failing_capabilities.length > 0
                ? `Failing capabilities: ${host.failing_capabilities.join(", ")}`
                : "This host has never reported a capability vector; placement soft-passes it."
            }
          >
            {host.failing_capabilities.length > 0 && <StatusDot tone="critical" size={6} />}
            {caps}
          </div>
        )}
      </TableCell>
      <TableCell className="align-top">
        <div className="flex flex-col gap-1.5">
          {host.running_sandboxes > 0 && (
            <div
              className="flex flex-wrap gap-0.5"
              aria-hidden
              data-testid={`sandbox-cells-${host.id}`}
            >
              {Array.from({ length: Math.min(host.running_sandboxes, MAX_CELLS) }, (_, i) => (
                // Resident sandboxes take the running tone, never lime — these
                // are state, and lime means something you can do.
                <span key={i} className="size-[9px] rounded-[2px] bg-ring" />
              ))}
            </div>
          )}
          <div className="font-mono text-2xs tabular-nums text-muted-foreground">
            {host.running_sandboxes}
            {host.running_sandboxes > MAX_CELLS && ` (+${host.running_sandboxes - MAX_CELLS})`}
            {inFlight.length > 0 && ` · ${inFlight.join(" · ")}`}
          </div>
        </div>
      </TableCell>
      <TableCell className="align-top">
        <Meter value={memPct} label="memory" />
        <div className="mt-1.5 font-mono text-2xs tabular-nums text-muted-foreground">
          {memPct === null
            ? "—"
            : `${gib(host.util_mem_used_mib)}/${gib(host.util_mem_total_mib)} GiB`}
        </div>
      </TableCell>
      <TableCell className="align-top">
        <Meter value={diskPct} label="disk" />
        <div className="mt-1.5 font-mono text-2xs tabular-nums text-muted-foreground">
          {diskPct === null
            ? "—"
            : `${gib(host.util_disk_used_mib)}/${gib(host.util_disk_total_mib)} GiB`}
        </div>
      </TableCell>
      <TableCell className="align-top">
        <Meter value={host.util_cpu_pct} label="cpu" />
        <div className="mt-1.5 font-mono text-2xs tabular-nums text-muted-foreground">
          {Math.round(host.util_cpu_pct)}%
        </div>
      </TableCell>
      <TableCell className="align-top text-right">
        {host.status === "ready" && (
          <AlertDialog>
            <AlertDialogTrigger asChild>
              <Button variant="ghost" size="sm" className="h-7" disabled={busy}>
                Drain
              </Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>Drain {host.id}?</AlertDialogTitle>
                <AlertDialogDescription>
                  The scheduler stops assigning new sandboxes to this host. Sandboxes already on it
                  finish in place.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Cancel</AlertDialogCancel>
                <AlertDialogAction onClick={onDrain}>Drain</AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        )}
        {host.status === "draining" && (
          // Reversible and immediate: nothing moves, the scheduler simply
          // starts picking the host again.
          <Button variant="ghost" size="sm" className="h-7" disabled={busy} onClick={onUndrain}>
            Undrain
          </Button>
        )}
      </TableCell>
    </TableRow>
  );
}
