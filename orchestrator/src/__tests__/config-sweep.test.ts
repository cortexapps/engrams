import { describe, expect, test } from "bun:test";

import { loadConfig } from "../config.ts";
import {
  HEARTBEAT_INTERVAL_MS,
  SWEEP_GRACE_MS,
  SWEEP_INTERVAL_MS,
} from "../sweep/sweeper.ts";

const BASE = { NODE_ENV: "test" } as Record<string, string | undefined>;

describe("config — DBOS orphan sweep", () => {
  test("uses safe production defaults", () => {
    const config = loadConfig({ ...BASE });

    expect(config.sweepDisabled).toBe(false);
    expect(config.sweepAlertChannel).toBe("");
    expect(config.sweepIntervalMs).toBe(SWEEP_INTERVAL_MS);
    expect(config.sweepGraceMs).toBe(SWEEP_GRACE_MS);
    expect(config.sweepHeartbeatIntervalMs).toBe(HEARTBEAT_INTERVAL_MS);
  });

  test("accepts explicit alert channel and positive interval overrides", () => {
    const config = loadConfig({
      ...BASE,
      ORCHESTRATOR_SWEEP_ALERT_CHANNEL: "C012OPS",
      ORCHESTRATOR_SWEEP_INTERVAL_MS: "1500",
      ORCHESTRATOR_SWEEP_GRACE_MS: "2500.5",
      ORCHESTRATOR_SWEEP_HEARTBEAT_INTERVAL_MS: "750",
    });

    expect(config.sweepAlertChannel).toBe("C012OPS");
    expect(config.sweepIntervalMs).toBe(1500);
    expect(config.sweepGraceMs).toBe(2500.5);
    expect(config.sweepHeartbeatIntervalMs).toBe(750);
  });

  test.each([
    ["ORCHESTRATOR_SWEEP_INTERVAL_MS", "not-a-number"],
    ["ORCHESTRATOR_SWEEP_INTERVAL_MS", "0"],
    ["ORCHESTRATOR_SWEEP_GRACE_MS", "-1"],
    ["ORCHESTRATOR_SWEEP_HEARTBEAT_INTERVAL_MS", ""],
    ["ORCHESTRATOR_SWEEP_HEARTBEAT_INTERVAL_MS", "Infinity"],
  ])("%s=%j is a hard configuration error", (key, value) => {
    expect(() => loadConfig({ ...BASE, [key]: value })).toThrow(
      new RegExp(key),
    );
  });

  test("rejects a grace window smaller than twice the heartbeat interval", () => {
    // A healthy pod between beats must never look dead: the grace window has
    // to absorb a missed beat plus pod-termination grace.
    expect(() =>
      loadConfig({
        ...BASE,
        ORCHESTRATOR_SWEEP_GRACE_MS: "15000",
        ORCHESTRATOR_SWEEP_HEARTBEAT_INTERVAL_MS: "30000",
      }),
    ).toThrow(/ORCHESTRATOR_SWEEP_GRACE_MS/);
  });

  test("rejects a too-small default grace when only the heartbeat is raised", () => {
    expect(() =>
      loadConfig({
        ...BASE,
        ORCHESTRATOR_SWEEP_HEARTBEAT_INTERVAL_MS: "400000",
      }),
    ).toThrow(/ORCHESTRATOR_SWEEP_GRACE_MS/);
  });

  test("accepts a grace window exactly twice the heartbeat interval", () => {
    const config = loadConfig({
      ...BASE,
      ORCHESTRATOR_SWEEP_GRACE_MS: "60000",
      ORCHESTRATOR_SWEEP_HEARTBEAT_INTERVAL_MS: "30000",
    });

    expect(config.sweepGraceMs).toBe(60_000);
    expect(config.sweepHeartbeatIntervalMs).toBe(30_000);
  });

  test.each(["1", "true"])(
    "ORCHESTRATOR_SWEEP_DISABLED=%s enables the kill switch",
    (value) => {
      expect(
        loadConfig({
          ...BASE,
          ORCHESTRATOR_SWEEP_DISABLED: value,
        }).sweepDisabled,
      ).toBe(true);
    },
  );

  test.each(["0", "false", "TRUE", "yes"])(
    "ORCHESTRATOR_SWEEP_DISABLED=%s does not enable the kill switch",
    (value) => {
      expect(
        loadConfig({
          ...BASE,
          ORCHESTRATOR_SWEEP_DISABLED: value,
        }).sweepDisabled,
      ).toBe(false);
    },
  );
});
