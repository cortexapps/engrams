/**
 * Small display chips for the integrations surfaces (redesign).
 *
 *  - AccessTag  — a read/write tag derived from an operation's method. Write is
 *    caution-tinted; color is never the sole carrier (always icon + text).
 *  - HostChip   — a mono host pill; `derived` = opened by a granted power.
 *  - StatusDot  — a status telltale (dot + optional label); pairs color with ink.
 */

import { EyeIcon, GlobeIcon, PencilIcon } from "lucide-react";

import { cn } from "@/lib/utils";
import type { Access } from "@/lib/connectorModel";

export function AccessTag({ access, className }: { access: Access; className?: string }) {
  const write = access === "write";
  const Icon = write ? PencilIcon : EyeIcon;
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 rounded-full border px-2 py-0.5 font-display text-[0.6rem] font-semibold tracking-[0.07em] uppercase",
        write
          ? "border-instrument-caution/50 bg-instrument-caution/12 text-foreground"
          : "border-border bg-secondary text-muted-foreground",
        className,
      )}
    >
      <Icon
        className={cn("size-3", write ? "text-instrument-caution" : "text-muted-foreground")}
        aria-hidden
      />
      {write ? "Write" : "Read"}
    </span>
  );
}

export function HostChip({
  host,
  derived,
  className,
}: {
  host: string;
  derived?: boolean;
  className?: string;
}) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-0.5 font-mono text-xs",
        derived
          ? "border-border bg-secondary text-foreground"
          : "border-dashed bg-transparent text-muted-foreground",
        className,
      )}
    >
      <GlobeIcon className="size-3 shrink-0 text-muted-foreground" aria-hidden />
      {host}
    </span>
  );
}

export type StatusTone = "nominal" | "caution" | "muted";

export function StatusDot({
  tone = "nominal",
  label,
  className,
}: {
  tone?: StatusTone;
  label?: string;
  className?: string;
}) {
  const dot =
    tone === "nominal"
      ? "bg-instrument-nominal"
      : tone === "caution"
        ? "bg-instrument-caution"
        : "bg-muted-foreground";
  return (
    <span className={cn("inline-flex items-center gap-1.5", className)}>
      <span className={cn("size-1.5 rounded-full", dot)} aria-hidden />
      {label && (
        <span className="font-display text-[0.6rem] font-semibold tracking-[0.08em] text-muted-foreground uppercase">
          {label}
        </span>
      )}
    </span>
  );
}
