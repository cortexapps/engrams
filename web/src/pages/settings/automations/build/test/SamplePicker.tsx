/** Which delivery to test against (ADR 0119 phase 3.4).
 *
 * Event triggers pick from the stored sample ledger (or paste a payload);
 * cron/manual triggers have no payload — they pick a `scheduled_for` instant
 * instead. The selection lives in editor state and rides every
 * TestRender / EvalCode / DryRun call. */

import { useState } from "react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import type { EventSample } from "@/gen/engram/app/v1/automation_pb";
import type { TestSample } from "@/hooks/useAutomationTest";

/** Compact "received" stamp for the dropdown; the list page's relativeTime
 * helper lives on a sibling branch (3.2), so this stays self-contained. */
function receivedLabel(iso: string, now: Date = new Date()): string {
  const then = new Date(iso);
  if (Number.isNaN(then.getTime())) return iso;
  const s = Math.max(0, Math.round((now.getTime() - then.getTime()) / 1000));
  if (s < 60) return "just now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h ago`;
  return then.toISOString().slice(0, 10);
}

export interface SamplePickerProps {
  /** Cron/manual triggers: no samples, pick an instant. */
  timed: boolean;
  samples: readonly EventSample[];
  loading?: boolean;
  value: TestSample;
  onChange: (next: TestSample) => void;
  now?: Date;
}

const PASTE = "__paste__";

/** A `datetime-local` input speaks the browser's wall clock; the stored
 * `scheduledFor` is a UTC instant. Display must convert back to local, or
 * every round-trip shifts the shown time by the UTC offset. Exported for the
 * test. */
export function toLocalDateTimeInput(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return new Date(d.getTime() - d.getTimezoneOffset() * 60_000).toISOString().slice(0, 16);
}

export function SamplePicker({ timed, samples, loading, value, onChange, now }: SamplePickerProps) {
  const [pasting, setPasting] = useState(value.kind === "payload");
  const [payloadText, setPayloadText] = useState(value.kind === "payload" ? value.payloadJson : "");

  if (timed) {
    const scheduled = value.kind === "scheduled" ? value.scheduledFor : "";
    return (
      <div className="flex flex-wrap items-center gap-2" data-testid="sample-picker">
        <label className="text-muted-foreground text-xs" htmlFor="test-scheduled-for">
          Scheduled for
        </label>
        <Input
          id="test-scheduled-for"
          type="datetime-local"
          className="h-8 w-56"
          value={scheduled ? toLocalDateTimeInput(scheduled) : ""}
          onChange={(e) => {
            const local = e.target.value;
            if (!local) {
              onChange({ kind: "none" });
              return;
            }
            onChange({ kind: "scheduled", scheduledFor: new Date(local).toISOString() });
          }}
        />
        <Button
          type="button"
          variant="ghost"
          size="sm"
          onClick={() =>
            onChange({ kind: "scheduled", scheduledFor: (now ?? new Date()).toISOString() })
          }
        >
          Now
        </Button>
      </div>
    );
  }

  const selectValue =
    value.kind === "sample" ? value.sampleId : value.kind === "payload" ? PASTE : "";

  return (
    <div className="flex flex-col gap-2" data-testid="sample-picker">
      <div className="flex flex-wrap items-center gap-2">
        <label className="text-muted-foreground text-xs" htmlFor="test-sample">
          Sample
        </label>
        <Select
          value={selectValue}
          onValueChange={(next) => {
            if (next === PASTE) {
              setPasting(true);
              onChange(
                payloadText ? { kind: "payload", payloadJson: payloadText } : { kind: "none" },
              );
              return;
            }
            setPasting(false);
            onChange({ kind: "sample", sampleId: next });
          }}
        >
          <SelectTrigger id="test-sample" className="h-8 w-80" aria-label="Sample">
            <SelectValue
              placeholder={
                loading
                  ? "Loading samples…"
                  : samples.length === 0
                    ? "No samples yet"
                    : "Pick a delivery"
              }
            />
          </SelectTrigger>
          <SelectContent>
            {samples.map((s) => (
              <SelectItem key={s.id} value={s.id}>
                <span className="font-mono text-xs">{s.eventKey}</span>
                <span className="text-muted-foreground ml-2 text-xs">
                  {receivedLabel(s.receivedAt, now)}
                </span>
              </SelectItem>
            ))}
            <SelectItem value={PASTE}>Paste a payload…</SelectItem>
          </SelectContent>
        </Select>
      </div>
      {pasting ? (
        <Textarea
          aria-label="Payload JSON"
          className="min-h-24 font-mono text-xs"
          placeholder='{"action": "opened", ...}'
          value={payloadText}
          onChange={(e) => {
            setPayloadText(e.target.value);
            onChange(
              e.target.value.trim()
                ? { kind: "payload", payloadJson: e.target.value }
                : { kind: "none" },
            );
          }}
        />
      ) : null}
    </div>
  );
}
