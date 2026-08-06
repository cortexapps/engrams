import { Link } from "@tanstack/react-router";
import { ArrowRight } from "lucide-react";
import { useHosts } from "../../hooks/useHosts";
import { useTasksAsSessionList } from "../../hooks/useTasks";
import { useStorageSummary } from "../../hooks/useStorageSummary";
import { fmtBytes } from "../../format";
import { PageHeading } from "../../components/page-heading";
import { StatReadout } from "../../components/stat-readout";
import { Gauge, type Tone, type Zone } from "../../components/gauge";
import { Text } from "@/components/ui/text";
import { cn } from "@/lib/utils";
import { deriveHealthMetrics, operatorIssues, type HealthMetrics } from "../../operator-health";
import { RESIDENT_SESSION_STATES, UNKNOWN_STATE, type SessionListItem } from "../../lib/types";

// The Operator cockpit: a read-only instrument cluster that answers "is the
// platform healthy?" in one read, then hands off to the detail surfaces. The
// master verdict is the headline; the two dials carry the continuous numbers
// (capacity, locality); the telltale strip lights up discrete faults; the stat
// readouts below are the trip-computer with the exact figures. No controls —
// every action (drain, GC) lives on the page it belongs to.

const CAPACITY_ZONES: Zone[] = [
  { to: 70, tone: "nominal" },
  { to: 90, tone: "caution" },
  { to: 100, tone: "critical" },
];
// Locality reads the other way: high is good, so the bands invert.
const LOCALITY_ZONES: Zone[] = [
  { to: 50, tone: "critical" },
  { to: 80, tone: "caution" },
  { to: 100, tone: "nominal" },
];

interface Verdict {
  tone: Tone | "neutral";
  headline: string;
  more: number;
}

export function Overview() {
  const { data: hosts } = useHosts();
  const { data: sessions } = useTasksAsSessionList();
  const { data: storage } = useStorageSummary();

  const h = hosts ?? [];
  const s = sessions ?? [];

  // Health figures (capacity, locality, fault counts) come from the shared
  // derivation so the dials, telltales, and verdict all read the same numbers
  // as the rail signal. The rest are display-only extras the cockpit shows.
  const m = deriveHealthMetrics(h, storage);
  const ready = h.filter((x) => x.status === "ready").length;
  // Resident = the VM exists and holds host RAM (parked/paused included) —
  // an Active-only count read "0 live sandboxes" on a fleet full of parked
  // VMs (status-set audit finding 8).
  // An `unknown` row (the control plane did not answer) is not counted as
  // resident: we cannot claim a VM holds RAM when we could not ask.
  const liveSandboxes = s.filter(
    (x: SessionListItem) => x.status !== UNKNOWN_STATE && RESIDENT_SESSION_STATES.has(x.status),
  ).length;
  const gcPending = storage?.gc_pending ?? 0;

  const verdict = computeVerdict(h.length === 0, m);

  const telltales: TelltaleProps[] = [
    { label: "Host down", tone: "critical", count: m.dead, lit: m.dead > 0 },
    { label: "Draining", tone: "caution", count: m.draining, lit: m.draining > 0 },
    { label: "At capacity", tone: "critical", lit: m.capPct >= 90 },
    { label: "RPO stale", tone: "caution", count: m.rpoStale, lit: m.rpoStale > 0 },
    { label: "Caps failing", tone: "caution", count: m.capsFailing, lit: m.capsFailing > 0 },
  ];

  const fleet = [
    { label: "Hosts ready", value: `${ready}/${h.length}` },
    { label: "Live sandboxes", value: liveSandboxes },
    { label: "Capacity used", value: `${m.capPct}%` },
    { label: "Draining", value: m.draining },
  ];
  const durability = [
    { label: "Snapshots", value: storage?.snapshots ?? 0 },
    { label: "Unflushed", value: fmtBytes(storage?.unflushed_bytes ?? 0) },
    { label: "Avg locality", value: m.locality == null ? "—" : `${m.locality}%` },
    { label: "GC pending", value: gcPending },
  ];

  return (
    <div className="space-y-8">
      <PageHeading title="Operator" />

      <VerdictBanner verdict={verdict} />

      {/* The instrument binnacle: two dials and the annunciator strip in one
          framed panel — not nested cards, a single cluster. */}
      <section className="rounded-lg border bg-card p-5 md:p-6">
        <div className="flex flex-col gap-8 md:flex-row md:items-center">
          <div className="grid flex-1 grid-cols-2 place-items-center gap-4">
            {/* Capacity is always a real reading (0% when the fleet is empty),
                so it matches the digital readout. Only locality goes blank —
                it has no value until something is chunk-tracked. */}
            <Gauge value={m.capPct} label="Capacity" zones={CAPACITY_ZONES} />
            <Gauge value={m.locality} label="Locality" zones={LOCALITY_ZONES} />
          </div>
          <div className="grid grid-cols-2 gap-x-6 gap-y-3 md:flex md:w-44 md:flex-col md:gap-2.5 md:border-l md:border-border md:pl-6">
            {telltales.map((t) => (
              <Telltale key={t.label} {...t} />
            ))}
          </div>
        </div>
      </section>

      <Cluster label="Fleet" to="/operator/fleet" linkLabel="View fleet">
        <StatReadout items={fleet} />
      </Cluster>

      <Cluster label="Storage durability" to="/operator/storage" linkLabel="View storage">
        <StatReadout items={durability} />
      </Cluster>
    </div>
  );
}

// The master verdict. The threshold judgement lives in operator-health.ts (shared
// with the rail telltale); here we only frame the worst issue as a headline and
// count the rest, plus the two states that aren't "an issue": no fleet, all clear.
function computeVerdict(noHosts: boolean, m: HealthMetrics): Verdict {
  if (noHosts) return { tone: "neutral", headline: "No hosts registered", more: 0 };

  const issues = operatorIssues(m);
  if (issues.length === 0) return { tone: "nominal", headline: "All systems nominal", more: 0 };

  const top = issues[0];
  return {
    tone: top.tone,
    headline: `${top.tone === "critical" ? "Critical" : "Caution"} · ${top.text}`,
    more: issues.length - 1,
  };
}

const DOT_BG: Record<Tone, string> = {
  nominal: "bg-instrument-nominal",
  caution: "bg-instrument-caution",
  critical: "bg-instrument-critical",
};

function VerdictBanner({ verdict }: { verdict: Verdict }) {
  const live = verdict.tone === "caution" || verdict.tone === "critical";
  const dotClass =
    verdict.tone === "neutral"
      ? "bg-muted-foreground/50"
      : verdict.tone === "nominal"
        ? DOT_BG.nominal
        : DOT_BG[verdict.tone];
  const tint = live
    ? {
        backgroundColor: `color-mix(in oklch, var(--color-instrument-${verdict.tone}) 7%, var(--card))`,
        borderColor: `color-mix(in oklch, var(--color-instrument-${verdict.tone}) 28%, var(--border))`,
      }
    : undefined;

  return (
    <div className="flex items-center gap-3 rounded-lg border px-4 py-3" style={tint}>
      <span className="relative flex size-2.5 shrink-0">
        {live && (
          <span
            aria-hidden
            className="absolute inline-flex size-full animate-ping rounded-full opacity-60 motion-reduce:hidden"
            style={{ backgroundColor: `var(--color-instrument-${verdict.tone})` }}
          />
        )}
        <span className={cn("relative inline-flex size-2.5 rounded-full", dotClass)} />
      </span>
      <Text variant="label" tone="default" className="text-[0.7rem]">
        {verdict.headline}
      </Text>
      {verdict.more > 0 && (
        <span className="font-mono text-xs tabular-nums text-muted-foreground">
          +{verdict.more} more
        </span>
      )}
    </div>
  );
}

interface TelltaleProps {
  label: string;
  tone: Tone;
  count?: number;
  lit: boolean;
}

function Telltale({ label, tone, count, lit }: TelltaleProps) {
  return (
    <div className="flex items-center gap-2">
      <span
        className={cn(
          "size-2 shrink-0 rounded-full transition-colors",
          lit ? DOT_BG[tone] : "bg-muted-foreground/25",
        )}
        style={lit ? { boxShadow: `0 0 6px 0 var(--color-instrument-${tone})` } : undefined}
        aria-hidden
      />
      <Text as="span" variant="label" tone={lit ? "default" : "muted"} className="text-[0.62rem]">
        {label}
      </Text>
      {count != null && (
        <span
          className={cn(
            "ml-auto font-mono text-xs tabular-nums",
            lit ? "text-foreground" : "text-muted-foreground/50",
          )}
        >
          {count}
        </span>
      )}
    </div>
  );
}

function Cluster({
  label,
  to,
  linkLabel,
  children,
}: {
  label: string;
  to: "/operator/fleet" | "/operator/storage";
  linkLabel: string;
  children: React.ReactNode;
}) {
  return (
    <section className="space-y-2">
      <div className="flex items-baseline justify-between">
        <Text as="h2" variant="label" tone="muted">
          {label}
        </Text>
        <Link
          to={to}
          className="inline-flex items-center gap-1 text-xs font-medium text-muted-foreground underline-offset-4 hover:text-foreground hover:underline"
        >
          {linkLabel}
          <ArrowRight className="size-3" />
        </Link>
      </div>
      {children}
    </section>
  );
}
