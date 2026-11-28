import { readFile } from "node:fs/promises";
import type { WorkerOptions } from "./service.ts";
import { createWorker } from "./service.ts";
import { ZonePolicy } from "./network.ts";
import { validateEgressRules } from "./deployment.ts";
import { object } from "./encoding.ts";
import type { ZoneConfig } from "./contracts.ts";

async function main() {
  const raw: unknown = JSON.parse(
    await readFile("/etc/mcp-worker/config.json", "utf8"),
  );
  if (
    !object(raw) ||
    !isZone(raw.zone) ||
    !Array.isArray(raw.callers) ||
    !Array.isArray(raw.egressRules)
  )
    throw Error("invalid_config");
  const callers = raw.callers.map((c): WorkerOptions["callers"][number] => {
    if (
      !object(c) ||
      typeof c.fingerprint !== "string" ||
      !/^([A-F0-9]{2}:){31}[A-F0-9]{2}$/.test(c.fingerprint) ||
      (c.role !== "orchestrator" && c.role !== "coordinator")
    )
      throw Error("invalid_caller");
    return { fingerprint: c.fingerprint, role: c.role };
  });
  const egress = raw.egressRules.map((rule) => {
    if (!object(rule) || typeof rule.cidr !== "string" || !strings(rule.except))
      throw Error("invalid_egress");
    return { cidr: rule.cidr, except: rule.except };
  });
  validateEgressRules(
    egress,
    raw.zone.denyCidrs,
    raw.zone.cidrs,
    raw.zone.mode,
  );
  const tls = {
    key: await readFile("/etc/mcp-worker/tls/tls.key", "utf8"),
    cert: await readFile("/etc/mcp-worker/tls/tls.crt", "utf8"),
    ca: await readFile("/etc/mcp-worker/tls/ca.crt", "utf8"),
  };
  const worker = createWorker({
    zone: new ZonePolicy(raw.zone),
    tls,
    callers,
    enabled: process.env.MCP_WORKER_ENABLED === "true",
  });
  await worker.listen();
  for (const signal of ["SIGTERM", "SIGINT"] as const)
    process.once(signal, () => {
      void worker.close().then(() => process.exit(0));
    });
}
function strings(v: unknown): v is string[] {
  return Array.isArray(v) && v.every((s) => typeof s === "string");
}
function isZone(v: unknown): v is ZoneConfig {
  if (
    !object(v) ||
    typeof v.id !== "string" ||
    typeof v.revision !== "string" ||
    !["public", "internal"].includes(String(v.mode)) ||
    !strings(v.authorities) ||
    !strings(v.aliases) ||
    !strings(v.cidrs) ||
    !strings(v.denyCidrs) ||
    (v.ca !== undefined && typeof v.ca !== "string")
  )
    return false;
  if (
    v.limits !== undefined &&
    (!object(v.limits) ||
      Object.entries(v.limits).some(
        ([key, value]) =>
          !["execute", "discover", "oauth", "schema", "toolMs"].includes(key) ||
          !Number.isSafeInteger(value) ||
          Number(value) < 1,
      ))
  )
    return false;
  return true;
}
process.on("uncaughtException", () => {
  process.stderr.write("worker_failed\n");
  process.exit(1);
});
process.on("unhandledRejection", () => {
  process.stderr.write("worker_failed\n");
  process.exit(1);
});
void main().catch(() => {
  process.stderr.write("worker_start_failed\n");
  process.exit(1);
});
