import { expect, test } from "bun:test";
import { makeMcpOperatorRuntime } from "../mcp/operator-runtime.ts";
import { operatorPublication } from "./mcp-binding-fixture.ts";
import type { McpRuntimeConfiguration } from "../mcp/runtime-config.ts";

test("worker selection requires fresh publication, exact destination, and both switches", async () => {
  const tls = {
    url: "https://worker.example.test/",
    ca: "ca",
    cert: "cert",
    key: "key",
    serverFingerprint: "AA",
  };
  const configuration: McpRuntimeConfiguration = {
    infrastructure: operatorPublication,
    control: tls,
    registrations: [],
    workers: new Map([["public", { enabled: true, deploymentId: "zone-workers", tls }]]),
  };
  let enabled = true,
    available = true,
    reads = 0;
  const policy = {
    publication: operatorPublication,
    zoneId: "public",
    policyRevision: "1",
    policyDigest: `sha256:${"f".repeat(64)}`,
    authorities: ["tools.example.test"],
    aliases: [],
  };
  const runtime = makeMcpOperatorRuntime({
    configuration,
    enabled: () => enabled,
    read: async () => {
      reads++;
      if (!available) throw Error("pending");
      return {
        generation: 1,
        identity: operatorPublication.identity,
        policies: [structuredClone(policy)],
      };
    },
  });
  const config = {
    endpoint: "https://tools.example.test/mcp",
    zone: "public",
    auth: { kind: "api_key" as const, secretRef: "" },
  };
  const selected = await runtime.select(config);
  expect(selected.deploymentId).toBe("zone-workers");
  await expect(
    runtime.select({ ...config, endpoint: "https://alias.example.test/mcp" }),
  ).rejects.toThrow();
  policy.publication = { ...operatorPublication, generation: 2 };
  await expect(runtime.assertCurrent(config, selected.policy)).rejects.toThrow("changed");
  available = false;
  await expect(runtime.select(config)).rejects.toThrow("pending");
  expect(reads).toBe(4);
  enabled = false;
  await expect(runtime.select(config)).rejects.toThrow("disabled");
  enabled = true;
  configuration.workers.get("public")!.enabled = false;
  await expect(runtime.select(config)).rejects.toThrow("disabled");
  expect(reads).toBe(4);
});
