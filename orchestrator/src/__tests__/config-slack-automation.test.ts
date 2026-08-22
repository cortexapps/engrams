import { describe, expect, test } from "bun:test";

import { loadConfig } from "../config.ts";

const BASE = { NODE_ENV: "test" } as Record<string, string | undefined>;

describe("config — Slack automation window kill switch (ADR 0119 phase 4.6)", () => {
  test("defaults to off", () => {
    expect(loadConfig({ ...BASE }).slackAutomationDisabled).toBe(false);
  });

  test.each(["1", "true"])("ORCHESTRATOR_SLACK_AUTOMATION_DISABLED=%s enables the kill switch", (value) => {
    expect(
      loadConfig({ ...BASE, ORCHESTRATOR_SLACK_AUTOMATION_DISABLED: value }).slackAutomationDisabled,
    ).toBe(true);
  });

  test.each(["0", "false", "TRUE", "yes"])(
    "ORCHESTRATOR_SLACK_AUTOMATION_DISABLED=%s does not enable the kill switch",
    (value) => {
      expect(
        loadConfig({ ...BASE, ORCHESTRATOR_SLACK_AUTOMATION_DISABLED: value }).slackAutomationDisabled,
      ).toBe(false);
    },
  );
});
