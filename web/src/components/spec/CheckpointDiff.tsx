import { diffLines, diffWordsWithSpace, type Change } from "diff";

interface DiffSegment {
  kind: "prose" | "code";
  before: string;
  after: string;
}

export function CheckpointDiff({ before, after }: { before: string; after: string }) {
  const segments = buildCheckpointDiff(before, after);
  return (
    <div className="spec-checkpoint-diff" aria-label="Checkpoint comparison">
      {segments.map((segment, index) => {
        const changes =
          segment.kind === "code"
            ? diffLines(segment.before, segment.after)
            : diffWordsWithSpace(segment.before, segment.after);
        return (
          <pre
            className={`spec-diff-segment is-${segment.kind}`}
            data-diff-granularity={segment.kind === "code" ? "line" : "word"}
            key={`${segment.kind}-${index}`}
          >
            {changes.map((change, changeIndex) => (
              <DiffChange change={change} key={changeIndex} />
            ))}
          </pre>
        );
      })}
    </div>
  );
}

function DiffChange({ change }: { change: Change }) {
  if (change.added) return <ins>{change.value}</ins>;
  if (change.removed) return <del>{change.value}</del>;
  return <span>{change.value}</span>;
}

export function buildCheckpointDiff(before: string, after: string): DiffSegment[] {
  const beforeSegments = splitMarkdown(before);
  const afterSegments = splitMarkdown(after);
  const length = Math.max(beforeSegments.length, afterSegments.length);
  const result: DiffSegment[] = [];
  for (let index = 0; index < length; index += 1) {
    const left = beforeSegments[index];
    const right = afterSegments[index];
    const kind = left?.kind === "code" || right?.kind === "code" ? "code" : "prose";
    result.push({ kind, before: left?.text ?? "", after: right?.text ?? "" });
  }
  return result;
}

function splitMarkdown(markdown: string): Array<{ kind: "prose" | "code"; text: string }> {
  const result: Array<{ kind: "prose" | "code"; text: string }> = [];
  const lines = markdown.replace(/\r\n/g, "\n").split(/(?<=\n)/);
  let kind: "prose" | "code" = "prose";
  let buffer = "";
  for (const line of lines) {
    const fence = line.startsWith("```");
    if (fence && kind === "prose") {
      if (buffer) result.push({ kind, text: buffer });
      kind = "code";
      buffer = line;
      continue;
    }
    buffer += line;
    if (fence && kind === "code") {
      result.push({ kind, text: buffer });
      kind = "prose";
      buffer = "";
    }
  }
  if (buffer) result.push({ kind, text: buffer });
  return result;
}
