/** Line diff between two automation versions' definitions (phase 3.8). */

import { diffLines } from "diff";

function pretty(json: string): string {
  try {
    return JSON.stringify(JSON.parse(json), null, 2);
  } catch {
    return json;
  }
}

export interface VersionDiffProps {
  beforeLabel: string;
  afterLabel: string;
  beforeJson: string;
  afterJson: string;
}

/** Pure helper for tests: the line-level changes, insertions/removals only. */
export function definitionDiff(beforeJson: string, afterJson: string) {
  return diffLines(pretty(beforeJson), pretty(afterJson));
}

export function VersionDiff({ beforeLabel, afterLabel, beforeJson, afterJson }: VersionDiffProps) {
  const changes = definitionDiff(beforeJson, afterJson);
  const changed = changes.some((c) => c.added || c.removed);
  return (
    <div
      className="flex flex-col gap-2"
      aria-label={`diff ${beforeLabel} to ${afterLabel}`}
      data-testid="version-diff"
    >
      <p className="text-muted-foreground text-xs">
        {beforeLabel} → {afterLabel}
        {!changed && " — identical definitions"}
      </p>
      <div className="overflow-x-auto rounded-lg border">
        <pre className="font-mono text-xs leading-5">
          {changes.map((change, i) => {
            const tone = change.added
              ? "bg-[color:var(--instrument-nominal)]/15"
              : change.removed
                ? "bg-[color:var(--instrument-critical)]/15"
                : "";
            const marker = change.added ? "+" : change.removed ? "-" : " ";
            return change.value
              .replace(/\n$/, "")
              .split("\n")
              .map((line, j) => (
                <div
                  key={`${i}-${j}`}
                  className={`px-3 ${tone}`}
                  data-diff={change.added ? "added" : change.removed ? "removed" : "same"}
                >
                  <span className="text-muted-foreground mr-2 select-none">{marker}</span>
                  {line}
                </div>
              ));
          })}
        </pre>
      </div>
    </div>
  );
}
