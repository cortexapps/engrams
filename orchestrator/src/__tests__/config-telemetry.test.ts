/**
 * config.ts telemetry-sink loading (ENGRAM_TELEMETRY_SINKS). Pure unit
 * tests: loadConfig() takes an explicit env map; NODE_ENV=test placeholders
 * the unrelated required vars.
 */

import { describe, expect, test } from "bun:test";
import { loadConfig, parseTelemetrySinks } from "../config.ts";

const BASE = { NODE_ENV: "test" } as Record<string, string | undefined>;

describe("config — telemetry sinks", () => {
  test("telemetry off when ENGRAM_TELEMETRY_SINKS unset / empty / []", () => {
    expect(loadConfig({ ...BASE }).telemetry).toBeUndefined();
    expect(loadConfig({ ...BASE, ENGRAM_TELEMETRY_SINKS: "  " }).telemetry).toBeUndefined();
    expect(loadConfig({ ...BASE, ENGRAM_TELEMETRY_SINKS: "[]" }).telemetry).toBeUndefined();
  });

  test("full sink list parses with per-sink defaults", () => {
    const cfg = loadConfig({
      ...BASE,
      ENGRAM_TELEMETRY_SINKS: JSON.stringify([
        {
          name: "langfuse",
          endpoint: "https://langfuse.corp/api/public/otel/v1/traces",
          headers: { Authorization: "Basic abc" },
          captureContent: true,
          serviceName: "engrams-prod",
        },
        { name: "collector", endpoint: "http://otel:4318/v1/traces" },
      ]),
    });
    expect(cfg.telemetry?.sinks).toEqual([
      {
        name: "langfuse",
        endpoint: "https://langfuse.corp/api/public/otel/v1/traces",
        headers: { Authorization: "Basic abc" },
        captureContent: true,
        serviceName: "engrams-prod",
      },
      {
        name: "collector",
        endpoint: "http://otel:4318/v1/traces",
        headers: {},
        captureContent: false, // default: metadata only
        serviceName: "engrams",
      },
    ]);
  });

  test("invalid JSON / non-array / bad entries are hard errors", () => {
    expect(() => parseTelemetrySinks("{nope")).toThrow(/not valid JSON/);
    expect(() => parseTelemetrySinks(`{"name":"x"}`)).toThrow(/must be a JSON array/);
    expect(() => parseTelemetrySinks(`[{"endpoint":"https://x"}]`)).toThrow(/name is required/);
    expect(() => parseTelemetrySinks(`[{"name":"a","endpoint":"not a url"}]`)).toThrow(
      /not a valid URL/,
    );
    expect(() => parseTelemetrySinks(`[{"name":"a","endpoint":"ftp://x/y"}]`)).toThrow(
      /must be http\(s\)/,
    );
  });

  test("duplicate sink names are rejected (they key durable cursors)", () => {
    expect(() =>
      parseTelemetrySinks(
        JSON.stringify([
          { name: "same", endpoint: "https://a.example/v1/traces" },
          { name: "same", endpoint: "https://b.example/v1/traces" },
        ]),
      ),
    ).toThrow(/duplicate sink name/);
  });
});
