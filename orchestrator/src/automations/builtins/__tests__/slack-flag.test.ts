import { beforeEach, describe, expect, test } from "bun:test";

import type { AutomationRow } from "../../../db/automations.ts";
import {
  SLACK_FLAG_CACHE_TTL_MS,
  invalidateSlackFlagCache,
  isChannelOnAutomation,
} from "../slack-flag.ts";
import { SLACK_BRAIN_DEFINITION } from "../slack-brain.ts";

const NOW = new Date("2026-08-21T00:00:00Z");

/** A full row: only `enabled` and `inputs.channels` matter to the flag. */
function rowWith(enabled: boolean, channels: Record<string, unknown>): AutomationRow {
  const d = SLACK_BRAIN_DEFINITION;
  return {
    id: "auto-slack",
    name: "Slack thread brain",
    description: "",
    enabled,
    kind: "builtin",
    builtinKey: "slack_brain",
    currentVersion: 1,
    inputs: { channels },
    blockOverrides: {},
    endSessionsOnFinish: false,
    nextFireAt: null,
    lastFiredAt: null,
    createdByUserId: null,
    createdAt: NOW,
    updatedAt: NOW,
    archivedAt: null,
    version: {
      automationId: "auto-slack",
      version: 1,
      trigger: d.trigger,
      blocks: d.blocks,
      inputsSchema: d.inputsSchema,
      settings: d.settings,
      createdByUserId: null,
      createdAt: NOW,
    },
  };
}

describe("isChannelOnAutomation", () => {
  let reads = 0;
  let row: AutomationRow | null = null;
  let now = 1_000_000;
  const deps = {
    store: {
      async getByBuiltinKey() {
        reads += 1;
        return row;
      },
    },
    nowMs: () => now,
  };

  beforeEach(() => {
    invalidateSlackFlagCache();
    reads = 0;
    now = 1_000_000;
  });

  test("enabled + channel in the map → on; other channels and a disabled row → off", async () => {
    row = rowWith(true, { C1: "prof-a" });
    expect(await isChannelOnAutomation("C1", deps)).toBe(true);
    expect(await isChannelOnAutomation("C2", deps)).toBe(false);
    invalidateSlackFlagCache();
    row = rowWith(false, { C1: "prof-a" });
    expect(await isChannelOnAutomation("C1", deps)).toBe(false);
  });

  test("no seeded row → off", async () => {
    row = null;
    expect(await isChannelOnAutomation("C1", deps)).toBe(false);
  });

  test("one read per TTL window; invalidation re-reads at once", async () => {
    row = rowWith(true, { C1: "p" });
    await isChannelOnAutomation("C1", deps);
    await isChannelOnAutomation("C1", deps);
    await isChannelOnAutomation("C9", deps);
    expect(reads).toBe(1);
    now += SLACK_FLAG_CACHE_TTL_MS;
    await isChannelOnAutomation("C1", deps);
    expect(reads).toBe(2);
    invalidateSlackFlagCache();
    row = rowWith(true, { C1: "p", C2: "p" });
    expect(await isChannelOnAutomation("C2", deps)).toBe(true);
    expect(reads).toBe(3);
  });

  test("a cold-cache burst reads once (single flight)", async () => {
    row = rowWith(true, { C1: "p" });
    const answers = await Promise.all([
      isChannelOnAutomation("C1", deps),
      isChannelOnAutomation("C1", deps),
      isChannelOnAutomation("C2", deps),
    ]);
    expect(answers).toEqual([true, true, false]);
    expect(reads).toBe(1);
  });
});
