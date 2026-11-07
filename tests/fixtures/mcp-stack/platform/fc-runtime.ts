import { constants } from "node:fs";
import { randomBytes } from "node:crypto";
import {
  lstat,
  open,
  readFile,
  readdir,
  readlink,
  realpath,
  stat,
  appendFile,
} from "node:fs/promises";
import { basename, dirname, join } from "node:path";
import { check, hash, put, readOwned } from "./owned.ts";
import type { Inputs } from "./preflight.ts";
import { parseHostRecord } from "../../../../orchestrator/scripts/mcp-stack/host-observer.ts";

const uuid = /^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/;
const absent = (error: unknown) =>
  error !== null &&
  typeof error === "object" &&
  "code" in error &&
  error.code === "ENOENT";
async function optional(path: string) {
  try {
    return await readFile(path, "utf8");
  } catch (error) {
    if (absent(error)) return undefined;
    throw error;
  }
}
export function nbdCount(value: string): number {
  check(/^[1-9][0-9]{0,3}\n?$/.test(value), "fc_nbd_module_count");
  const count = Number(value.trim());
  check(count <= 4096, "fc_nbd_module_count");
  return count;
}
export function procBirth(value: string, pid: number): string {
  const close = value.lastIndexOf(")");
  const fields = value
    .slice(close + 1)
    .trim()
    .split(/\s+/);
  check(
    value.startsWith(`${pid} (`) &&
      close > 0 &&
      /^[0-9]+$/.test(fields[19] ?? ""),
    "fc_process_birth",
  );
  return fields[19]!;
}
export function linuxDevice(value: bigint): string {
  return `${((value >> 8n) & 0xfffn) | ((value >> 32n) & 0xfffff000n)}:${(value & 0xffn) | ((value >> 12n) & 0xffffff00n)}`;
}
export type FcProcess = {
  pid: number;
  bootId: string;
  birth: string;
  uid: number;
  executable: string;
  cgroup: string;
  sha256: string;
};
export type NbdSlot = {
  path: string;
  ino: string;
  rdev: string;
  device: string;
  uid: number;
};
export type FcInventory = {
  bootId: string;
  release: string;
  pidNamespace: string;
  slots: NbdSlot[];
  readSectors: Record<string, string>;
};
export function verifyFcProcessIdentity(
  expected: FcProcess,
  actual: FcProcess | undefined,
) {
  check(
    actual && JSON.stringify(actual) === JSON.stringify(expected),
    "fc_process_identity_changed",
  );
}
async function kernelIdentity() {
  const bootId = (
    await readFile("/proc/sys/kernel/random/boot_id", "utf8")
  ).trim();
  const release = (await readFile("/proc/sys/kernel/osrelease", "utf8")).trim();
  const pidNamespace = await readlink("/proc/1/ns/pid");
  check(
    uuid.test(bootId) &&
      /^[A-Za-z0-9._+-]{1,256}$/.test(release) &&
      /^pid:\[[0-9]+\]$/.test(pidNamespace) &&
      (await readlink("/proc/self/ns/pid")) === pidNamespace,
    "fc_initial_kernel",
  );
  return { bootId, release, pidNamespace };
}
async function pids(): Promise<number[]> {
  const values = (await readdir("/proc")).filter((v) =>
    /^[1-9][0-9]*$/.test(v),
  );
  check(values.length <= 65536, "fc_process_inventory_bound");
  return values.map(Number);
}
async function processIdentity(
  pid: number,
  bootId: string,
  digest = false,
): Promise<FcProcess | undefined> {
  try {
    const path = `/proc/${pid}`;
    const before = await readFile(`${path}/stat`, "utf8");
    const executable = await readlink(`${path}/exe`);
    const status = await readFile(`${path}/status`, "utf8");
    const uids = /^Uid:\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)$/m.exec(status);
    const cgroup = await readFile(`${path}/cgroup`, "utf8");
    check(uids && /^0::\/[^\n]*\n$/.test(cgroup), "fc_process_identity");
    const sha256 = digest ? hash(await readFile(`${path}/exe`)) : "";
    const birth = procBirth(before, pid);
    check(
      procBirth(await readFile(`${path}/stat`, "utf8"), pid) === birth &&
        (await readlink(`${path}/exe`)) === executable,
      "fc_process_changed",
    );
    return {
      pid,
      bootId,
      birth,
      uid: Number(uids[1]),
      executable,
      cgroup: cgroup.trim().slice(3),
      sha256,
    };
  } catch (error) {
    if (absent(error)) return undefined;
    throw error;
  }
}
async function slotIdentity(index: number): Promise<NbdSlot> {
  const path = `/dev/nbd${index}`;
  const node = await lstat(path, { bigint: true });
  const device = (await readFile(`/sys/block/nbd${index}/dev`, "utf8")).trim();
  check(
    node.isBlockDevice() &&
      !node.isSymbolicLink() &&
      node.uid === 0n &&
      (await realpath(path)) === path &&
      /^[0-9]+:[0-9]+$/.test(device) &&
      linuxDevice(node.rdev) === device,
    "fc_nbd_device_identity",
  );
  return {
    path,
    ino: String(node.ino),
    rdev: String(node.rdev),
    device,
    uid: Number(node.uid),
  };
}
async function assertSlotIdentity(slot: NbdSlot) {
  const actual = await slotIdentity(Number(slot.path.slice(8)));
  check(
    JSON.stringify(actual) === JSON.stringify(slot),
    "fc_nbd_device_changed",
  );
}
async function deviceFds(slots: NbdSlot[], processes: number[]) {
  const devices = new Set(slots.map((v) => v.rdev));
  const found: { pid: number; fd: string; rdev: string }[] = [];
  for (const pid of processes) {
    let fds: string[];
    try {
      fds = await readdir(`/proc/${pid}/fd`);
    } catch (error) {
      if (absent(error)) continue;
      throw error;
    }
    check(fds.length <= 16384, "fc_fd_inventory_bound");
    for (const fd of fds) {
      check(/^[0-9]+$/.test(fd), "fc_fd_identity");
      try {
        const state = await stat(`/proc/${pid}/fd/${fd}`, { bigint: true });
        if (state.isBlockDevice() && devices.has(String(state.rdev)))
          found.push({ pid, fd, rdev: String(state.rdev) });
      } catch (error) {
        if (!absent(error)) throw error;
      }
    }
  }
  return found;
}
async function assertFree(slot: NbdSlot) {
  await assertSlotIdentity(slot);
  const sys = `/sys/block/${basename(slot.path)}`;
  check(
    (await optional(`${sys}/pid`)) === undefined &&
      (await readFile(`${sys}/size`, "utf8")).trim() === "0" &&
      (await readdir(`${sys}/holders`)).length === 0,
    "fc_nbd_busy",
  );
  // Linux block devices honor O_EXCL without O_CREAT. A failed open is a hard
  // boundary; this code never changes node modes or disconnects a binding.
  const file = await open(
    slot.path,
    constants.O_RDWR | constants.O_EXCL | constants.O_NOFOLLOW,
  );
  try {
    const actual = await file.stat({ bigint: true });
    check(
      actual.isBlockDevice() &&
        String(actual.ino) === slot.ino &&
        String(actual.rdev) === slot.rdev,
      "fc_nbd_exclusive_identity",
    );
  } finally {
    await file.close();
  }
  check(
    (await optional(`${sys}/pid`)) === undefined &&
      (await readFile(`${sys}/size`, "utf8")).trim() === "0",
    "fc_nbd_busy",
  );
}
export async function inspectFcInventory(
  inputs: Pick<Inputs, "binaries" | "proxy" | "firecracker">,
): Promise<FcInventory> {
  const identity = await kernelIdentity();
  const processes = await pids();
  const names = new Set([
    "engram-host-agent",
    "firecracker",
    "engram-egress-proxyd",
    "engram-uffd-handler",
    basename(inputs.binaries.hostAgent.path),
    basename(inputs.binaries.helper.path),
    basename(inputs.proxy.path),
    ...(inputs.firecracker ? [basename(inputs.firecracker.path)] : []),
  ]);
  for (const pid of processes) {
    const current = await processIdentity(pid, identity.bootId);
    check(
      !current || !names.has(basename(current.executable)),
      "fc_runner_not_exclusive",
    );
  }
  const count = nbdCount(
    await readFile("/sys/module/nbd/parameters/nbds_max", "utf8"),
  );
  const slots: NbdSlot[] = [];
  for (let i = 0; i < count; i++) {
    const slot = await slotIdentity(i);
    await assertFree(slot);
    slots.push(slot);
  }
  check(
    (await deviceFds(slots, await pids())).length === 0,
    "fc_nbd_foreign_fd",
  );
  const readSectors: Record<string, string> = {};
  for (const slot of slots) {
    const fields = (
      await readFile(`/sys/block/${basename(slot.path)}/stat`, "utf8")
    )
      .trim()
      .split(/\s+/);
    check(
      fields.length >= 11 && /^[0-9]+$/.test(fields[2]!),
      "fc_nbd_read_counter",
    );
    readSectors[slot.path] = fields[2]!;
  }
  return { ...identity, slots, readSectors };
}
/** Read-only proof for the dedicated whole kernel. Device cgroups do not
 * isolate NBD netlink control. Runner exclusivity is an operator prerequisite. */
export class FcRuntime {
  private readonly cgroups = new Set<string>();
  private readonly processes = new Map<string, FcProcess>();
  private readonly observations = new Set<string>();
  private evidenceWriteFailed = false;
  private host: FcProcess | undefined;
  private readonly readBaseline = new Map<string, string>();
  private readonly deviceOwners = new Map<string, string>();
  private readonly deviceBoots = new Map<string, string>();
  constructor(
    readonly inventory: FcInventory,
    private readonly inputs: Inputs,
    private readonly root: string,
    private readonly appendEvidence = (path: string, bytes: string) =>
      appendFile(path, bytes, { mode: 0o600 }),
  ) {
    for (const [path, reads] of Object.entries(inventory.readSectors))
      this.readBaseline.set(path, reads);
  }
  deviceOptions() {
    return this.inventory.slots.flatMap((slot) => [
      "--device",
      `${slot.path}:${slot.path}:rw`,
    ]);
  }
  environment() {
    return {
      ENGRAM_NBD_FIRST_SLOT: "0",
      ENGRAM_NBD_MAX_SLOTS: String(this.inventory.slots.length),
      ENGRAM_NBD_WARM_SLOTS: "1",
    };
  }
  async qualifyKernel(pid: number) {
    check(
      nbdCount(
        await readFile("/sys/module/nbd/parameters/nbds_max", "utf8"),
      ) === this.inventory.slots.length,
      "fc_nbd_module_changed",
    );
    const mounts = (await readFile(`/proc/${pid}/mountinfo`, "utf8"))
      .trim()
      .split("\n")
      .map((line) => line.split(" "));
    for (const target of ["/proc/sys", "/sys", "/sys/fs/cgroup"])
      check(
        mounts.some(
          (fields) =>
            fields[4] === target && fields[5]?.split(",").includes("ro"),
        ),
        "fc_host_readonly_kernel_mount",
      );
    check(
      JSON.stringify(await kernelIdentity()) ===
        JSON.stringify({
          bootId: this.inventory.bootId,
          release: this.inventory.release,
          pidNamespace: this.inventory.pidNamespace,
        }),
      "fc_kernel_changed",
    );
    check(
      (await readlink(`/proc/${pid}/ns/pid`)) === this.inventory.pidNamespace &&
        (await readlink(`/proc/${pid}/ns/cgroup`)) !==
          (await readlink("/proc/self/ns/cgroup")),
      "fc_host_kernel_namespace",
    );
    const current = await processIdentity(pid, this.inventory.bootId);
    check(
      current && current.uid === 0 && current.cgroup !== "/",
      "fc_host_cgroup",
    );
    this.cgroups.add(current.cgroup);
    this.processes.set(`${pid}:${current.birth}`, current);
    for (const slot of this.inventory.slots) {
      await assertSlotIdentity(slot);
      const projected = await lstat(`/proc/${pid}/root${slot.path}`, {
        bigint: true,
      });
      check(
        projected.isBlockDevice() && String(projected.rdev) === slot.rdev,
        "fc_host_device_projection",
      );
    }
  }
  async adoptHost(pid: number) {
    await this.qualifyKernel(pid);
    const host = await processIdentity(pid, this.inventory.bootId, true);
    check(
      host &&
        host.uid === 0 &&
        host.sha256 === this.inputs.binaries.hostAgent.sha256 &&
        host.executable === "/usr/local/bin/engram-host-agent" &&
        host.cgroup !== "/",
      "fc_host_process",
    );
    const newHost =
      !this.host ||
      this.host.pid !== host.pid ||
      this.host.birth !== host.birth;
    this.host = host;
    this.cgroups.add(host.cgroup);
    this.processes.set(`${host.pid}:${host.birth}`, host);
    if (newHost)
      await put(
        join(this.root, `fc-host-${host.pid}-${host.birth}.json`),
        JSON.stringify({ inventory: this.inventory, host }),
      );
  }
  private owns(cgroup: string) {
    return [...this.cgroups].some(
      (value) => cgroup === value || cgroup.startsWith(value + "/"),
    );
  }
  async observe() {
    check(!this.evidenceWriteFailed, "fc_physical_evidence_write");
    if (!this.host) return;
    const identities = new Map<number, FcProcess>();
    const all = await pids();
    for (const pid of all) {
      const current = await processIdentity(pid, this.inventory.bootId);
      if (!current || !this.owns(current.cgroup)) continue;
      const full = await processIdentity(pid, this.inventory.bootId, true);
      if (!full) continue;
      identities.set(pid, full);
      this.processes.set(`${pid}:${full.birth}`, full);
    }
    const fds = await deviceFds(this.inventory.slots, all);
    check(
      fds.every((fd) => identities.has(fd.pid)),
      "fc_nbd_foreign_fd",
    );
    for (const slot of this.inventory.slots) {
      const sys = `/sys/block/${basename(slot.path)}`;
      if ((await optional(`${sys}/pid`)) !== undefined) continue;
      const fields = (await readFile(`${sys}/stat`, "utf8"))
        .trim()
        .split(/\s+/);
      check(
        fields.length >= 11 && /^[0-9]+$/.test(fields[2]!),
        "fc_nbd_read_counter",
      );
      this.readBaseline.set(slot.path, fields[2]!);
      this.deviceOwners.delete(slot.path);
      this.deviceBoots.delete(slot.path);
    }
    const work = join(this.root, "host");
    for (const name of (await readdir(work)).filter((v) => uuid.test(v))) {
      let bootBytes: Buffer | undefined;
      try {
        bootBytes = await readOwned(
          join(work, "managed-boots", `${name}.boot`),
          16 * 1024 * 1024,
        );
      } catch (error) {
        if (!absent(error)) throw error;
      }
      if (!bootBytes) continue;
      const boot = parseHostRecord(bootBytes, name).record;
      check(
        boot.request.key.context.target.host_id ===
          (await readOwned(join(work, "host_id"))).toString(),
        "fc_managed_host_identity",
      );
      let raw: string | undefined;
      try {
        raw = (
          await readOwned(join(work, name, "sandbox.json"), 1024 * 1024, false)
        ).toString();
      } catch (error) {
        if (!absent(error)) throw error;
      }
      if (raw === undefined) continue;
      check(raw.length <= 1024 * 1024, "fc_sandbox_manifest_bound");
      const manifest = JSON.parse(raw);
      check(
        manifest.schema_version === 3 &&
          manifest.backend === "firecracker" &&
          manifest.sandbox_id === name,
        "fc_sandbox_manifest",
      );
      const fc = manifest.firecracker;
      const process = identities.get(fc?.pid);
      if (!process) continue; // A normal destroy may have removed the process first.
      check(
        this.inputs.firecracker &&
          process.sha256 === this.inputs.firecracker.sha256 &&
          process.uid === 0 &&
          process.birth === String(fc.start_time_jiffies) &&
          process.executable === "/usr/local/bin/firecracker",
        "fc_sandbox_process",
      );
      check(
        typeof fc.rootfs_canonical === "string" &&
          dirname(fc.rootfs_canonical) === join(work, "rootfs") &&
          /^[a-f0-9-]{36}\.dev$/.test(basename(fc.rootfs_canonical)),
        "fc_rootfs_path",
      );
      const device = await realpath(fc.rootfs_canonical);
      const slot = this.inventory.slots.find((v) => v.path === device);
      check(slot, "fc_rootfs_not_nbd");
      await assertSlotIdentity(slot);
      const sys = `/sys/block/${basename(device)}`;
      const binding = (await optional(`${sys}/pid`))?.trim();
      const sectors = (await readFile(`${sys}/size`, "utf8")).trim();
      const io = (await readFile(`${sys}/stat`, "utf8")).trim().split(/\s+/);
      check(
        binding &&
          /^[1-9][0-9]*$/.test(binding) &&
          identities.has(Number(binding)) &&
          /^[1-9][0-9]*$/.test(sectors) &&
          io.length >= 11 &&
          /^[0-9]+$/.test(io[2]!),
        "fc_nbd_binding",
      );
      check(
        fds.some((fd) => fd.pid === process.pid && fd.rdev === slot.rdev),
        "fc_rootfs_device_fd",
      );
      const owner = (
        await readOwned(join(work, "nbd-owners", basename(device)), 1024, false)
      )
        .toString()
        .trim();
      check(owner === name, "fc_nbd_durable_owner");
      const key = `${name}:${boot.request.key.boot_attempt}`;
      const previousOwner = this.deviceOwners.get(slot.path);
      const previousBoot = this.deviceBoots.get(slot.path);
      if (
        (previousOwner && previousOwner !== owner) ||
        (previousBoot && previousBoot !== key)
      )
        this.readBaseline.set(slot.path, io[2]!);
      this.deviceOwners.set(slot.path, owner);
      this.deviceBoots.set(slot.path, key);
      const readSectorsBefore = this.readBaseline.get(slot.path);
      check(
        readSectorsBefore && /^[0-9]+$/.test(readSectorsBefore),
        "fc_nbd_read_baseline",
      );
      if (BigInt(io[2]!) <= BigInt(readSectorsBefore)) continue;
      if (this.observations.has(key)) continue;
      const finalProcess = await processIdentity(
        process.pid,
        this.inventory.bootId,
        true,
      );
      const bindingProcess = await processIdentity(
        Number(binding),
        this.inventory.bootId,
        true,
      );
      verifyFcProcessIdentity(process, finalProcess);
      const expectedBinding = identities.get(Number(binding));
      check(expectedBinding, "fc_nbd_binding_process");
      verifyFcProcessIdentity(expectedBinding, bindingProcess);
      await this.retainObservation(
        key,
        JSON.stringify({
          sandboxId: name,
          sessionId: boot.request.cold.session_id,
          bootAttempt: boot.request.key.boot_attempt,
          process,
          slot,
          bindingPid: Number(binding),
          sizeSectors: sectors,
          readSectorsBefore,
          readSectors: io[2],
          owner,
          rootfsCanonical: fc.rootfs_canonical,
          bootRecordSha256: hash(bootBytes),
        }) + "\n",
      );
    }
  }
  // The path is fixed. This seam permits controlled write-failure tests;
  // observe calls it only after all physical checks pass.
  async retainObservation(key: string, bytes: string) {
    check(!this.evidenceWriteFailed, "fc_physical_evidence_write");
    if (this.observations.has(key)) return;
    try {
      await this.appendEvidence(join(this.root, "fc-physical.jsonl"), bytes);
    } catch (error) {
      // A failed append can leave an incomplete record. Preserve that failure
      // for the whole run; a later successful sample cannot grant coverage.
      this.evidenceWriteFailed = true;
      throw error;
    }
    this.observations.add(key);
  }
  async requireCoverage() {
    check(!this.evidenceWriteFailed, "fc_physical_evidence_write");
    const files = await readdir(join(this.root, "host/managed-boots"));
    const boots = files.filter((v) => /^[a-f0-9-]{36}\.boot$/.test(v));
    check(boots.length > 0 && boots.length <= 4096, "fc_physical_boot_count");
    for (const file of boots) {
      const id = file.slice(0, -5);
      const boot = parseHostRecord(
        await readOwned(join(this.root, "host/managed-boots", file)),
        id,
      ).record;
      check(
        this.observations.has(`${id}:${boot.request.key.boot_attempt}`),
        "fc_physical_boot_unobserved",
      );
    }
  }
  async requireReleased() {
    check(
      nbdCount(
        await readFile("/sys/module/nbd/parameters/nbds_max", "utf8"),
      ) === this.inventory.slots.length,
      "fc_nbd_module_changed",
    );
    for (const slot of this.inventory.slots) await assertFree(slot);
    check(
      (await deviceFds(this.inventory.slots, await pids())).length === 0,
      "fc_nbd_remaining_fd",
    );
  }
  async requireStopped() {
    const survivors: FcProcess[] = [];
    for (const pid of await pids()) {
      const current = await processIdentity(pid, this.inventory.bootId);
      if (current && this.owns(current.cgroup)) survivors.push(current);
    }
    await put(
      join(
        this.root,
        `fc-process-cleanup-${randomBytes(8).toString("hex")}.json`,
      ),
      JSON.stringify({ observed: [...this.processes.values()], survivors }),
    );
    check(survivors.length === 0, "fc_owned_process_survivor");
    await this.requireReleased();
  }
}
