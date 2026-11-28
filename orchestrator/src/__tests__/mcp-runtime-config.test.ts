import { expect, test } from "bun:test";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { loadMcpRuntimeConfiguration } from "../mcp/runtime-config.ts";

test("runtime configuration defaults zones off and binds actual CA bytes at unchanged paths", async () => {
  const dir = await mkdtemp(join(tmpdir(), "mcp-config-"));
  try {
    await writeFile(join(dir, "ca"), "first-ca");
    await writeFile(join(dir, "cert"), "certificate");
    await writeFile(join(dir, "key"), "private-key", { mode: 0o600 });
    const tls = {
      url: "https://control.example.test/",
      caName: "control",
      certFile: join(dir, "cert"),
      keyFile: join(dir, "key"),
      serverFingerprint: Array(32).fill("AA").join(":"),
    };
    const config = {
      version: 1,
      infrastructure: {
        deploymentIdentity: `sha256:${"a".repeat(64)}`,
        networkIdentity: `sha256:${"b".repeat(64)}`,
        trustIdentity: `sha256:${"c".repeat(64)}`,
      },
      caFiles: { control: join(dir, "ca") },
      control: tls,
      workers: [{ zoneId: "public", deploymentId: "public-workers", tls }],
    };
    await writeFile(join(dir, "config"), JSON.stringify(config));
    const first = await loadMcpRuntimeConfiguration(join(dir, "config"));
    expect(first.workers.get("public")!.enabled).toBe(false);
    expect(first.destinations).toEqual([]);
    const configured = {
      ...config,
      destinations: [{ zoneId: "public", endpoint: "https://tools.example.test/custom/mount" }],
    };
    await writeFile(join(dir, "config"), JSON.stringify(configured));
    expect((await loadMcpRuntimeConfiguration(join(dir, "config"))).destinations).toEqual(
      configured.destinations,
    );
    for (const destinations of [
      [{ zoneId: "public", endpoint: "https://tools.example.test/rpc?query=bad" }],
      [{ zoneId: "public", endpoint: "https://tools.example.test/rpc", extra: true }],
      Array.from({ length: 129 }, () => configured.destinations[0]),
    ]) {
      await writeFile(join(dir, "config"), JSON.stringify({ ...config, destinations }));
      await expect(loadMcpRuntimeConfiguration(join(dir, "config"))).rejects.toThrow();
    }
    await writeFile(join(dir, "config"), JSON.stringify(config));
    await writeFile(join(dir, "ca"), "second-ca");
    const next = await loadMcpRuntimeConfiguration(join(dir, "config"));
    expect(next.infrastructure.caContentDigests.control).not.toBe(
      first.infrastructure.caContentDigests.control,
    );
    expect(next.control.ca).toBe("second-ca");
    config.workers.push(config.workers[0]!);
    await writeFile(join(dir, "config"), JSON.stringify(config));
    await expect(loadMcpRuntimeConfiguration(join(dir, "config"))).rejects.toThrow("duplicated");
  } finally {
    await rm(dir, { recursive: true });
  }
});
