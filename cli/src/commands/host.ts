/**
 * engrams host … — FleetService passthrough verbs (admin surfaces).
 *
 * `evacuate` closes the gap the Rust CLI documented ("no engram-cli surface
 * for EvacuateSession") — the passthrough forwards the whole FleetService.
 */

import type { Clients } from "../client.ts";
import { detail, failWith, printJson, table } from "../output.ts";
import type { HostView } from "../gen/engram/app/v1/fleet_pb.ts";

function hostJson(h: HostView) {
  return {
    id: h.id,
    hostname: h.hostname,
    status: h.status,
    cordoned: h.cordoned,
    // ADR 0046/0047 figures — what placement actually schedules on.
    // (The old capacity_used_mib was a dead legacy field: written as a
    // literal 0 everywhere, so `host list` read "fleet empty" while
    // creates queue-timed out — 2026-07-11 campaign.)
    allocatable_mib: Number(h.allocatableMib),
    reserved_mib: Number(h.reservedMib),
    free_mib: Number(h.freeMib),
    base_shm_mib: Number(h.utilBaseShmMib),
    cpu_budget_vcpus: Number(h.cpuBudgetVcpus),
    free_vcpus: Number(h.freeVcpus),
    failing_capabilities: h.failingCapabilities,
    running_sandboxes: h.runningSandboxes,
    ready_images: h.readyImages,
    ready_image_digests: h.readyImageDigests,
    last_heartbeat_at: h.lastHeartbeatAt,
  };
}

/** `ready*` — cordoned or capability-failed hosts don't take placements. */
function statusCell(h: HostView): string {
  const flagged = h.cordoned || h.failingCapabilities.length > 0;
  return flagged ? `${h.status}*` : h.status;
}

export async function list(c: Clients, json: boolean): Promise<void> {
  const resp = await c.fleet.listHosts({}).catch(failWith);
  if (json) {
    printJson({ hosts: resp.hosts.map(hostJson) });
    return;
  }
  if (resp.hosts.length === 0) {
    console.log("(no hosts registered)");
    return;
  }
  table(
    ["ID", "STATUS", "FREE_MIB", "ALLOC_MIB", "BASE_SHM", "VCPU_FREE", "SANDBOXES"],
    resp.hosts.map((h) => [
      h.id,
      statusCell(h),
      String(h.freeMib),
      String(h.allocatableMib),
      String(h.utilBaseShmMib),
      String(h.freeVcpus),
      String(h.runningSandboxes),
    ]),
    [36, 10, 9, 10, 9, 9, 9],
  );
  const flagged = resp.hosts.filter(
    (h) => h.cordoned || h.failingCapabilities.length > 0,
  );
  for (const h of flagged) {
    const why = [
      ...(h.cordoned ? ["cordoned"] : []),
      ...h.failingCapabilities.map((c) => `cap:${c}`),
    ].join(", ");
    console.log(`  * ${h.id}: ${why}`);
  }
}

export async function get(c: Clients, id: string, json: boolean): Promise<void> {
  const resp = await c.fleet.getHost({ hostId: id }).catch(failWith);
  const h = resp.host;
  if (!h) failWith(new Error("response carried no host"));
  if (json) {
    printJson(hostJson(h));
    return;
  }
  detail([
    ["id", h.id],
    ["hostname", h.hostname],
    ["status", h.status],
    ["cordoned", String(h.cordoned)],
    ["allocatable", `${h.allocatableMib} MiB`],
    ["reserved", `${h.reservedMib} MiB`],
    ["free", `${h.freeMib} MiB`],
    ["base_shm", `${h.utilBaseShmMib} MiB`],
    ["cpu_budget", `${h.cpuBudgetVcpus} vCPU`],
    ["cpu_free", `${h.freeVcpus} vCPU`],
    ["failing_capabilities", h.failingCapabilities.join(", ") || "(none)"],
    ["running_sandboxes", String(h.runningSandboxes)],
  ]);
}

export async function drain(c: Clients, id: string): Promise<void> {
  await c.fleet.drainHost({ hostId: id }).catch(failWith);
  console.log("draining");
}

/** FleetService.DeleteHost — deregister a host row. FAILED_PRECONDITION while
 *  any session is still bound (drain / reap sessions first); idempotent-success
 *  if already gone. The dev case: a dead leftover host-agent (e.g. an fc-colima
 *  VM's after `just dev-fc`) otherwise keeps feeding the fleet bundle catalog
 *  and winning placement — the dead-host probe dials its host_addr, reaches
 *  whatever now answers on that port, and "rescues" the row forever. */
export async function remove(c: Clients, id: string): Promise<void> {
  await c.fleet.deleteHost({ hostId: id }).catch(failWith);
  console.log("deleted");
}

export async function uncordon(c: Clients, id: string, json: boolean): Promise<void> {
  const resp = await c.fleet.uncordonHost({ hostId: id }).catch(failWith);
  if (json) printJson({ host_id: resp.hostId, status: resp.status });
  else console.log(`${resp.hostId}: ${resp.status}`);
}

export async function evacuate(c: Clients, sessionId: string, json: boolean): Promise<void> {
  const resp = await c.fleet.evacuateSession({ sessionId }).catch(failWith);
  if (json) printJson({ session_id: resp.sessionId, status: resp.status });
  else console.log(`${resp.sessionId}: ${resp.status}`);
}
