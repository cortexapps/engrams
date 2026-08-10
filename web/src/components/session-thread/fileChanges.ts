import { diffLines } from "diff";
import type { FileChange, IndexedEvent } from "../../events";

export interface FileChangeRollup {
  path: string;
  changes: { toolCallId: string; change: FileChange; at: string }[];
  additions: number;
  deletions: number;
}

// Reconstructs the before/after text of a unified diff from its hunk bodies.
//
// We scan the lines ourselves instead of calling jsdiff's `parsePatch` because
// that function validates each `@@` header's line counts against the hunk body
// and throws when they disagree. Real harnesses emit such diffs: the codex
// harness forwards the diff the codex CLI reports verbatim, and those headers
// are sometimes wrong (seen in prod: `@@ -1,3 +7,3 @@` above a body with 9 new
// lines). A throw here reached the render-time `useMemo` in every changes pane
// and blanked the whole session page. The header counts carry no information we
// need — only the leading +/-/space marker of each body line does — so we skip
// the headers and stay tolerant of a malformed one.
function splitUnifiedDiff(unifiedDiff: string): { before: string; after: string } {
  const before: string[] = [];
  const after: string[] = [];
  const lines = unifiedDiff.split("\n");
  let inHunk = false;

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!;

    if (line.startsWith("@@")) {
      inHunk = true;
      continue;
    }
    if (!inHunk) continue;

    // `--- a/x` immediately followed by `+++ b/x` is the next file's header,
    // not a delete/add pair. Only the first file belongs to this event's path.
    if (line.startsWith("--- ") && lines[i + 1]?.startsWith("+++ ")) break;

    if (line.startsWith("\\")) continue; // `\ No newline at end of file`

    // A bare empty line is a context line whose text is empty, except for the
    // final element produced by a trailing newline.
    const marker = line.length === 0 && i < lines.length - 1 ? " " : line[0];
    const text = line.slice(1);
    if (marker === " ") {
      before.push(text);
      after.push(text);
    } else if (marker === "-") {
      before.push(text);
    } else if (marker === "+") {
      after.push(text);
    } else {
      // Anything else (`diff --git`, `index …`, `Binary files …`, the trailing
      // empty element) ends this file's hunk bodies.
      break;
    }
  }

  return { before: before.join("\n"), after: after.join("\n") };
}

export function beforeAfter(change: FileChange): { before: string; after: string } {
  if (change.write) return { before: "", after: change.write.content };
  if (change.edit) {
    const hunks = change.edit.hunks;
    return {
      before: hunks.map((hunk) => hunk.old).join("\n"),
      after: hunks.map((hunk) => hunk.new).join("\n"),
    };
  }
  if (change.patch) return splitUnifiedDiff(change.patch.unified_diff);
  return { before: "", after: "" };
}

export function countChange(change: FileChange): { additions: number; deletions: number } {
  const { before, after } = beforeAfter(change);
  let additions = 0;
  let deletions = 0;
  for (const part of diffLines(before, after)) {
    if (part.added) additions += part.count ?? 0;
    else if (part.removed) deletions += part.count ?? 0;
  }
  return { additions, deletions };
}

export function totalCounts(rollups: FileChangeRollup[]): { additions: number; deletions: number } {
  let additions = 0;
  let deletions = 0;
  for (const rollup of rollups) {
    additions += rollup.additions;
    deletions += rollup.deletions;
  }
  return { additions, deletions };
}

export function extractFileChanges(events: IndexedEvent[]): FileChangeRollup[] {
  const rollups: FileChangeRollup[] = [];
  const byPath = new Map<string, FileChangeRollup>();

  for (const { event } of events) {
    if (event.type !== "file_changed") continue;
    let rollup = byPath.get(event.path);
    if (!rollup) {
      rollup = {
        path: event.path,
        changes: [],
        additions: 0,
        deletions: 0,
      };
      byPath.set(event.path, rollup);
      rollups.push(rollup);
    }
    const count = countChange(event.change);
    rollup.changes.push({ toolCallId: event.tool_call_id, change: event.change, at: event.at });
    rollup.additions += count.additions;
    rollup.deletions += count.deletions;
  }

  return rollups;
}
