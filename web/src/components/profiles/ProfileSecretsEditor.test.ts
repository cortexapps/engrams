import { describe, it, expect } from "vitest";
import {
  secretRowsToWire,
  wireToSecretRows,
  splitHosts,
  newSecretRow,
  type SecretRow,
} from "./ProfileSecretsEditor";

describe("ProfileSecretsEditor helpers (ADR 0057)", () => {
  it("splitHosts splits on commas, spaces, and newlines and drops blanks", () => {
    expect(splitHosts("a.com, b.com\n c.com  ,, ")).toEqual(["a.com", "b.com", "c.com"]);
    expect(splitHosts("")).toEqual([]);
  });

  it("secretRowsToWire drops ref-less rows and trims, broker carries allowHosts", () => {
    const rows: SecretRow[] = [
      {
        id: "1",
        ref: " datadog-api-key ",
        envVar: " DD_API_KEY ",
        mode: "broker",
        allowHostsText: "api.datadoghq.com",
      },
      { id: "2", ref: "", envVar: "IGNORED", mode: "literal", allowHostsText: "" }, // no ref → dropped
      {
        id: "3",
        ref: "db-url",
        envVar: "DATABASE_URL",
        mode: "literal",
        allowHostsText: "ignored.com",
      },
    ];
    expect(secretRowsToWire(rows)).toEqual([
      {
        ref: "datadog-api-key",
        envVar: "DD_API_KEY",
        mode: "broker",
        allowHosts: ["api.datadoghq.com"],
        allowHostPatterns: [],
      },
      // literal mode drops allowHosts (only broker substitutes at the proxy)
      {
        ref: "db-url",
        envVar: "DATABASE_URL",
        mode: "literal",
        allowHosts: [],
        allowHostPatterns: [],
      },
    ]);
  });

  it("wireToSecretRows hydrates rows, coercing an unknown mode to broker", () => {
    const rows = wireToSecretRows([
      { ref: "k", envVar: "K", mode: "weird", allowHosts: ["h1.com", "h2.com"] },
    ]);
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({
      ref: "k",
      envVar: "K",
      mode: "broker",
      allowHostsText: "h1.com, h2.com",
    });
    expect(rows[0].id).toBeTruthy();
  });

  it("newSecretRow defaults to a broker secret with a stable id", () => {
    const a = newSecretRow();
    const b = newSecretRow();
    expect(a.mode).toBe("broker");
    expect(a.id).not.toEqual(b.id);
  });
});
