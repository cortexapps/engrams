/** Version history with pick-two-to-diff (phase 3.8). Built-in versions
 * are shipped by engrams, so they read "shipped vN". */

import { useState } from "react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useAutomationVersions } from "@/hooks/useAutomationEditor";
import { relativeTime } from "@/lib/automations";

import { VersionDiff } from "./VersionDiff";

export interface VersionsListProps {
  automationId: string;
  builtin: boolean;
}

export function versionLabel(number: number, builtin: boolean): string {
  return builtin ? `shipped v${number}` : `v${number}`;
}

export function VersionsList({ automationId, builtin }: VersionsListProps) {
  const query = useAutomationVersions(automationId);
  const versions = [...(query.data?.versions ?? [])].sort((a, b) => b.number - a.number);
  const [picked, setPicked] = useState<number[]>([]);

  const toggle = (number: number) => {
    setPicked((prev) => {
      if (prev.includes(number)) return prev.filter((n) => n !== number);
      // Keep at most two; the newest pick replaces the oldest.
      return [...prev, number].slice(-2);
    });
  };

  const pair = picked.length === 2 ? [...picked].sort((a, b) => a - b) : null;
  const before = pair ? versions.find((v) => v.number === pair[0]) : undefined;
  const after = pair ? versions.find((v) => v.number === pair[1]) : undefined;

  return (
    <section className="flex flex-col gap-3" aria-label="versions">
      <div className="flex items-baseline justify-between">
        <h3 className="text-sm font-semibold">Versions</h3>
        <p className="text-muted-foreground text-xs">
          {picked.length === 2 ? "Comparing" : "Pick two to compare"}
        </p>
      </div>
      {versions.length === 0 ? (
        <p className="text-muted-foreground text-sm">No versions yet.</p>
      ) : (
        <ul className="divide-y rounded-lg border">
          {versions.map((v, i) => {
            const selected = picked.includes(v.number);
            return (
              <li key={v.number} className="flex items-center gap-3 px-3 py-2 text-sm">
                <Button
                  size="sm"
                  variant={selected ? "default" : "outline"}
                  aria-pressed={selected}
                  onClick={() => toggle(v.number)}
                  data-testid={`version-pick-${v.number}`}
                >
                  {versionLabel(v.number, builtin)}
                </Button>
                {i === 0 && <Badge variant="secondary">current</Badge>}
                <span className="text-muted-foreground ml-auto text-xs">
                  {relativeTime(v.createdAt)}
                </span>
              </li>
            );
          })}
        </ul>
      )}
      {before && after && (
        <VersionDiff
          beforeLabel={versionLabel(before.number, builtin)}
          afterLabel={versionLabel(after.number, builtin)}
          beforeJson={before.definitionJson}
          afterJson={after.definitionJson}
        />
      )}
    </section>
  );
}
