import { expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { effectFixture } from "./mcp-effect-fixture.ts";

test("failed worker version setup closes the child before the fixture rejects", async () => {
  const dir = await mkdtemp(join(tmpdir(), "mcp-effect-setup-"));
  const executable = join(dir, "worker"),
    pidFile = join(dir, "pid");
  const previous = process.env.MCP_WORKER_TEST_NODE;
  let pid: number | undefined;
  try {
    await writeFile(
      executable,
      `#!/usr/bin/env node
const fs = require("node:fs");
fs.writeFileSync(${JSON.stringify(pidFile)}, String(process.pid));
process.stdin.resume();
console.log(JSON.stringify({ node: "unsupported", port: 0, identity: {} }));
setInterval(() => {}, 1000);
`,
      { mode: 0o700 },
    );
    process.env.MCP_WORKER_TEST_NODE = executable;
    await expect(effectFixture()).rejects.toThrow();
    pid = Number(await readFile(pidFile, "utf8"));
    expect(() => process.kill(pid!, 0)).toThrow();
  } finally {
    if (previous === undefined) delete process.env.MCP_WORKER_TEST_NODE;
    else process.env.MCP_WORKER_TEST_NODE = previous;
    if (pid !== undefined) {
      try {
        process.kill(pid, "SIGKILL");
      } catch {
        /* Already reaped. */
      }
    }
    await rm(dir, { recursive: true, force: true });
  }
}, 10000);

test("spawn failure settles without waiting for a missing child exit", async () => {
  const previous = process.env.MCP_WORKER_TEST_NODE;
  try {
    process.env.MCP_WORKER_TEST_NODE = "/missing-mcp-fixture-node";
    await expect(effectFixture()).rejects.toThrow();
  } finally {
    if (previous === undefined) delete process.env.MCP_WORKER_TEST_NODE;
    else process.env.MCP_WORKER_TEST_NODE = previous;
  }
}, 10000);
