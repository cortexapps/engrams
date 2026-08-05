import { describe, expect, test } from "vitest";
import type { FileChange, IndexedEvent } from "../../events";
import { countChange, extractFileChanges } from "./fileChanges";

const AT = "2026-08-05T12:00:00.000Z";

function changed(idx: number, path: string, change: FileChange): IndexedEvent {
  return {
    idx,
    event: {
      type: "file_changed",
      run_id: "run-1",
      tool_call_id: `tool-${idx}`,
      path,
      change,
      at: `${AT}-${idx}`,
    },
  };
}

describe("countChange", () => {
  test("counts write, edit, and patch changes", () => {
    expect(countChange({ write: { content: "one\ntwo\n" } })).toEqual({
      additions: 2,
      deletions: 0,
    });
    expect(
      countChange({ edit: { hunks: [{ old: "keep\nold", new: "keep\nnew\nextra" }] } }),
    ).toEqual({ additions: 2, deletions: 1 });
    expect(
      countChange({
        patch: {
          unified_diff: "--- a/demo.txt\n+++ b/demo.txt\n@@ -1,2 +1,2 @@\n keep\n-old\n+new\n",
        },
      }),
    ).toEqual({ additions: 1, deletions: 1 });
  });
});

describe("extractFileChanges", () => {
  test("groups changes by path and sums their counts", () => {
    const changes = extractFileChanges([
      changed(0, "src/a.ts", { write: { content: "one\ntwo\n" } }),
      changed(1, "src/b.ts", { edit: { hunks: [{ old: "old", new: "new" }] } }),
      changed(2, "src/a.ts", { edit: { hunks: [{ old: "two", new: "three" }] } }),
    ]);

    expect(changes).toHaveLength(2);
    expect(changes[0]).toMatchObject({
      path: "src/a.ts",
      additions: 3,
      deletions: 1,
    });
    expect(changes[0]!.changes.map((change) => change.toolCallId)).toEqual(["tool-0", "tool-2"]);
    expect(changes[1]).toMatchObject({
      path: "src/b.ts",
      additions: 1,
      deletions: 1,
    });
  });

  test("keeps first-seen path order and event order within each path", () => {
    const changes = extractFileChanges([
      changed(0, "z.ts", { write: { content: "z" } }),
      changed(1, "a.ts", { write: { content: "a" } }),
      changed(2, "z.ts", { write: { content: "new z" } }),
    ]);

    expect(changes.map((change) => change.path)).toEqual(["z.ts", "a.ts"]);
    expect(changes[0]!.changes.map((change) => change.at)).toEqual([`${AT}-0`, `${AT}-2`]);
  });
});
