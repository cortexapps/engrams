import { diffLines, parsePatch } from "diff";
import type { FileChange, IndexedEvent } from "../../events";

export interface FileChangeRollup {
  path: string;
  changes: { toolCallId: string; change: FileChange; at: string }[];
  additions: number;
  deletions: number;
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
  if (change.patch) {
    const parsed = parsePatch(change.patch.unified_diff)[0];
    if (!parsed) return { before: "", after: "" };
    const before: string[] = [];
    const after: string[] = [];
    for (const hunk of parsed.hunks) {
      for (const line of hunk.lines) {
        if (line.startsWith("\\ No newline")) continue;
        const marker = line[0];
        const text = line.slice(1);
        if (marker !== "+") before.push(text);
        if (marker !== "-") after.push(text);
      }
    }
    return { before: before.join("\n"), after: after.join("\n") };
  }
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
