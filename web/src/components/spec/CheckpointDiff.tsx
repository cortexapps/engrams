import { diffLines, diffWordsWithSpace, type Change } from "diff";

interface DiffSegment {
  kind: "prose" | "code";
  granularity: "word" | "line";
  before: string;
  after: string;
}

interface MarkdownSegment {
  kind: "prose" | "code";
  text: string;
}

const MAX_ALIGNMENT_CELLS = 10_000;

export function CheckpointDiff({ before, after }: { before: string; after: string }) {
  const segments = buildCheckpointDiff(before, after);
  return (
    <div className="spec-checkpoint-diff" aria-label="Checkpoint comparison">
      {segments.map((segment, index) => {
        const changes =
          segment.granularity === "line"
            ? diffLines(segment.before, segment.after)
            : diffWordsWithSpace(segment.before, segment.after);
        return (
          <pre
            className={`spec-diff-segment is-${segment.kind}`}
            data-diff-granularity={segment.granularity}
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
  if ((beforeSegments.length + 1) * (afterSegments.length + 1) > MAX_ALIGNMENT_CELLS) {
    return [{ kind: "prose", granularity: "line", before, after }];
  }
  const scores = alignmentScores(beforeSegments, afterSegments);
  const result: DiffSegment[] = [];
  let leftIndex = 0;
  let rightIndex = 0;
  while (leftIndex < beforeSegments.length || rightIndex < afterSegments.length) {
    const left = beforeSegments[leftIndex];
    const right = afterSegments[rightIndex];
    const pairScore = left && right && left.kind === right.kind ? segmentScore(left, right) : -1;
    if (
      left &&
      right &&
      left.kind === right.kind &&
      scores[leftIndex]![rightIndex] === pairScore + scores[leftIndex + 1]![rightIndex + 1]
    ) {
      result.push({
        kind: left.kind,
        granularity: left.kind === "code" ? "line" : "word",
        before: left.text,
        after: right.text,
      });
      leftIndex += 1;
      rightIndex += 1;
    } else if (left && scores[leftIndex]![rightIndex] === scores[leftIndex + 1]![rightIndex] - 1) {
      result.push({
        kind: left.kind,
        granularity: left.kind === "code" ? "line" : "word",
        before: left.text,
        after: "",
      });
      leftIndex += 1;
    } else if (right) {
      result.push({
        kind: right.kind,
        granularity: right.kind === "code" ? "line" : "word",
        before: "",
        after: right.text,
      });
      rightIndex += 1;
    }
  }
  return result;
}

function alignmentScores(
  before: readonly MarkdownSegment[],
  after: readonly MarkdownSegment[],
): number[][] {
  const scores = Array.from({ length: before.length + 1 }, () =>
    Array<number>(after.length + 1).fill(0),
  );
  for (let left = before.length; left >= 0; left -= 1)
    scores[left]![after.length] = left - before.length;
  for (let right = after.length; right >= 0; right -= 1)
    scores[before.length]![right] = right - after.length;
  for (let left = before.length - 1; left >= 0; left -= 1) {
    for (let right = after.length - 1; right >= 0; right -= 1) {
      const beforeSegment = before[left]!;
      const afterSegment = after[right]!;
      const aligned =
        beforeSegment.kind === afterSegment.kind
          ? segmentScore(beforeSegment, afterSegment) + scores[left + 1]![right + 1]!
          : Number.NEGATIVE_INFINITY;
      scores[left]![right] = Math.max(
        aligned,
        scores[left + 1]![right]! - 1,
        scores[left]![right + 1]! - 1,
      );
    }
  }
  return scores;
}

function segmentScore(before: MarkdownSegment, after: MarkdownSegment): number {
  return before.text === after.text ? 4 : 1;
}

function splitMarkdown(markdown: string): MarkdownSegment[] {
  const result: MarkdownSegment[] = [];
  const lines = markdown.replace(/\r\n/g, "\n").split(/(?<=\n)/);
  let kind: "prose" | "code" = "prose";
  let buffer = "";
  for (const line of lines) {
    const fence = line.startsWith("```");
    if (fence && kind === "prose") {
      if (buffer.trim().length > 0) result.push({ kind, text: buffer });
      kind = "code";
      buffer = line;
      continue;
    }
    buffer += line;
    if (fence && kind === "code") {
      result.push({ kind, text: buffer });
      kind = "prose";
      buffer = "";
    } else if (kind === "prose" && line.trim().length === 0) {
      if (buffer.trim().length > 0) result.push({ kind, text: buffer });
      buffer = "";
    }
  }
  if (buffer.trim().length > 0) result.push({ kind, text: buffer });
  return result;
}
