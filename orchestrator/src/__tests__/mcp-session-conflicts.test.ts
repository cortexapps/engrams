import { expect, test } from "bun:test";
import { assertMcpDirectConflicts } from "../mcp/session-conflicts.ts";
import type { IntegrationPolicyJson } from "../connectors/registry.ts";
test("final direct policy rejects managed hosts, aliases, patterns and authoritative secret references", () => {
  const base: IntegrationPolicyJson = {
    injects: [],
    observes: [],
    network: { default: "allow", allow_hosts: [], allow_host_patterns: [] },
    secrets: [],
    guest_services: [],
    tunnels: [],
  };
  const hosts = ["tools.example.test", "alias.example.test"],
    reserved = new Set(["reserved-reference"]);
  expect(() => assertMcpDirectConflicts(base, hosts, reserved)).not.toThrow();
  for (const host of hosts)
    expect(() =>
      assertMcpDirectConflicts(
        { ...base, network: { ...base.network, allow_hosts: [host] } },
        hosts,
        reserved,
      ),
    ).toThrow("conflicts");
  expect(() =>
    assertMcpDirectConflicts(
      { ...base, network: { ...base.network, allow_host_patterns: ["*.example.test"] } },
      hosts,
      reserved,
    ),
  ).toThrow("conflicts");
  expect(() =>
    assertMcpDirectConflicts(
      {
        ...base,
        secrets: [
          {
            secret_ref: "reserved-reference",
            env_var: "TOKEN",
            mode: "literal",
            allow_hosts: [],
            allow_host_patterns: [],
          },
        ],
      },
      hosts,
      reserved,
    ),
  ).toThrow("credential");
});
