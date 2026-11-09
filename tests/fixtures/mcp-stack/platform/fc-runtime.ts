import { randomBytes } from "node:crypto";
import {
  lstat,
  readFile,
  readdir,
  readlink,
  stat,
  appendFile,
} from "node:fs/promises";
import { basename, dirname, join } from "node:path";
import { check, hash, put, readOwned } from "./owned.ts";
import type { Inputs } from "./preflight.ts";
import {
  admitPool,
  OfflinePoolAccess,
  verifyThinIds,
  requirePoolBudget,
  verifyBootDevice,
  verifyReleasedMappings,
  inspectPool,
  verifyPoolIdentity,
  deviceNode,
  type PoolInventory,
  type DeviceNode,
  type Backing,
} from "./fc-thin.ts";
export { linuxDevice } from "./fc-thin.ts";
import {
  parseHostRecord,
  type HostRecord,
} from "../../../../orchestrator/scripts/mcp-stack/host-observer.ts";

const uuid = /^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/;
const absent = (error: unknown) =>
  error !== null &&
  typeof error === "object" &&
  "code" in error &&
  error.code === "ENOENT";
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
export type FcProcess = {
  pid: number;
  bootId: string;
  birth: string;
  uid: number;
  executable: string;
  cgroup: string;
  sha256: string;
};
export type FcInventory = {
  bootId: string;
  release: string;
  pidNamespace: string;
  pool: PoolInventory;
};
function managedNeedsIo(
  record: Pick<HostRecord, "finalization" | "state" | "capture">,
) {
  check(
    [
      "missing",
      "prepared",
      "finalized",
      "start_pending",
      "started",
      "failed",
      "captured",
      "retired",
    ].includes(String(record.state.state)),
    "fc_managed_boot_state",
  );
  // Finalization is persisted before start and is never cleared on failure,
  // retirement or capture. Terminal state alone cannot prove never-started.
  return (
    record.finalization !== null ||
    record.capture !== null ||
    ["finalized", "start_pending", "started", "captured"].includes(
      String(record.state.state),
    )
  );
}
export function requireManagedBootCoverage(
  required: Set<string>,
  observed: ReadonlySet<string>,
  key: string,
  record: Pick<HostRecord, "finalization" | "state" | "capture">,
) {
  if (managedNeedsIo(record)) required.add(key);
  check(!required.has(key) || observed.has(key), "fc_physical_boot_unobserved");
}
export function managedBootPaths(
  work: string,
  sandbox: string,
  attempt: string,
) {
  check(
    uuid.test(sandbox) && /^[A-Za-z0-9-]{1,128}$/.test(attempt),
    "fc_managed_boot_identity",
  );
  const directory = join(
    work,
    `m-${hash(`${sandbox}\0${attempt}`).slice(0, 24)}`,
  );
  return {
    directory,
    apiSocket: join(directory, "firecracker.sock"),
    rootfsCanonical: join(directory, "rootfs.ext4"),
    volume: `sb-${sandbox.replaceAll("-", "")}-root`,
  };
}
export function selectManagedProcess(
  candidates: { process: FcProcess; args: string[] }[],
  apiSocket: string,
): FcProcess | undefined {
  const matches = candidates.filter((candidate) =>
    candidate.args.includes(apiSocket),
  );
  check(matches.length <= 1, "fc_managed_process_duplicate");
  const match = matches[0];
  if (!match) return undefined;
  check(
    JSON.stringify(match.args) ===
      JSON.stringify(["/usr/local/bin/firecracker", "--api-sock", apiSocket]),
    "fc_managed_process_arguments",
  );
  return match.process;
}
async function processArguments(pid: number) {
  const value = await readFile(`/proc/${pid}/cmdline`);
  check(
    value.length > 0 && value.length <= 4096 && value.at(-1) === 0,
    "fc_process_arguments",
  );
  return value.subarray(0, -1).toString().split("\0");
}
type BootCandidate = {
  name: string;
  process: FcProcess;
  rootfsCanonical: string;
  boot?: HostRecord;
  bootBytes?: Buffer;
  sandboxManifest?: string;
  apiSocket?: string;
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
async function deviceFds(
  slots: DeviceNode[],
  processes: number[],
  files: Backing[] = [],
) {
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
        if (
          ((state.isBlockDevice() || state.isCharacterDevice()) &&
            devices.has(String(state.rdev))) ||
          (state.isFile() &&
            files.some(
              (file) =>
                String(state.dev) === file.fileDev &&
                String(state.ino) === file.fileIno,
            ))
        )
          found.push({ pid, fd, rdev: String(state.rdev) });
      } catch (error) {
        if (!absent(error)) throw error;
      }
    }
  }
  return found;
}
export async function inspectFcInventory(
  inputs: Pick<Inputs, "binaries" | "proxy" | "firecracker" | "thinPool">,
): Promise<FcInventory> {
  const identity = await kernelIdentity();
  check(inputs.thinPool, "fc_pool_required");
  const initial = await inspectPool(inputs.thinPool);
  const pool = await new OfflinePoolAccess().inspect(
    () => requireStorageQuiescence(inputs, identity.bootId, initial),
    (lease) => inspectPool(initial, lease),
  );
  pool.fds = await deviceFds(
    [pool.data, pool.metadata, await deviceNode(pool.path)],
    await pids(),
    [pool.data, pool.metadata],
  );
  admitPool(pool);
  return { ...identity, pool };
}
async function requireStorageQuiescence(
  inputs: Pick<Inputs, "binaries" | "proxy" | "firecracker">,
  bootId: string,
  pool: PoolInventory,
) {
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
  const processes = await pids();
  for (const pid of processes) {
    const current = await processIdentity(pid, bootId);
    check(
      !current || !names.has(basename(current.executable)),
      "fc_storage_process_live",
    );
  }
  check(
    (
      await deviceFds(
        [pool.control, pool.data, pool.metadata, pool.node],
        processes,
        [pool.data, pool.metadata],
      )
    ).length === 0,
    "fc_storage_fd_live",
  );
}

export function verifyFcKernelPosture(mountinfo: string, status: string) {
  const mounts = mountinfo
    .trim()
    .split("\n")
    .map((line) => line.split(" "));
  for (const target of ["/sys", "/sys/fs/cgroup"])
    check(
      mounts.some(
        (fields) =>
          fields[4] === target && fields[5]?.split(",").includes("ro"),
      ),
      "fc_host_readonly_kernel_mount",
    );
  const procMounts = mounts
    .filter((fields) => fields[4] === "/proc" || fields[4] === "/proc/sys")
    .sort((left, right) => right[4]!.length - left[4]!.length);
  const effectiveProc = procMounts[0];
  check(
    effectiveProc &&
      procMounts.filter((fields) => fields[4] === effectiveProc[4]).length ===
        1 &&
      effectiveProc[5]?.split(",").includes("rw") &&
      !effectiveProc[5]?.split(",").includes("ro") &&
      effectiveProc[effectiveProc.indexOf("-") + 1] === "proc",
    "fc_host_writable_proc_sys",
  );
  const capabilities = (1n << 10n) | (1n << 12n) | (1n << 21n) | (1n << 27n);
  for (const field of ["CapEff", "CapPrm", "CapBnd"]) {
    const value = new RegExp(`^${field}:\\s+([a-f0-9]+)$`, "m").exec(
      status,
    )?.[1];
    check(
      value && BigInt("0x" + value) === capabilities,
      "fc_host_effective_capabilities",
    );
  }
  check(
    /^NoNewPrivs:\s+1$/m.test(status) && /^Seccomp:\s+2$/m.test(status),
    "fc_host_effective_confinement",
  );
  return mounts;
}
/** The runner owns the whole disposable kernel. Device cgroups cannot
 * isolate device-mapper control from other host workloads. */
export class FcRuntime {
  private readonly storageAccess = new OfflinePoolAccess();
  private readonly cgroups = new Set<string>();
  private readonly processes = new Map<string, FcProcess>();
  private readonly observations = new Set<string>();
  private readonly requiredBoots = new Set<string>();
  private readonly observationKinds = new Set<"ordinary" | "managed">();
  private evidenceWriteFailed = false;
  private observation: Promise<void> = Promise.resolve();
  private host: FcProcess | undefined;
  private readonly bootDevices = new Map<string, string>();
  private readonly bootReads = new Map<string, bigint>();
  private readonly ownedSandboxes = new Set<string>();
  private readonly retainedVolumes = new Map<string, string>();
  constructor(
    readonly inventory: FcInventory,
    private readonly inputs: Inputs,
    private readonly root: string,
    private readonly appendEvidence = (path: string, bytes: string) =>
      appendFile(path, bytes, { mode: 0o600 }),
  ) {}
  deviceOptions() {
    const p = this.inventory.pool;
    return [
      "--device",
      "/dev/mapper/control:/dev/mapper/control:rw",
      "--device",
      `${p.metadata.path}:${p.metadata.path}:r`,
      "--device",
      `${p.path}:${p.path}:rw`,
      "--device-cgroup-rule",
      `b ${p.major}:* rwm`,
      "--cap-add",
      "MKNOD",
      "--mount",
      `type=bind,src=${join(this.root, "thin-nodes")},dst=/dev/engram`,
    ];
  }
  environment() {
    return { ENGRAM_THIN_POOL: this.inventory.pool.name };
  }
  private async proveStorageQuiescent() {
    // The offline lease stops new samples. Drain any table-reading subprocess
    // before checking control-device FDs, then inspect the actual kernel state.
    await this.observation.catch(() => {});
    check(!this.evidenceWriteFailed, "fc_physical_evidence_write");
    await requireStorageQuiescence(
      this.inputs,
      this.inventory.bootId,
      this.inventory.pool,
    );
  }
  async beforeHostStart() {
    const prove = () => this.proveStorageQuiescent();
    await this.storageAccess.stopped(prove);
    const current = await this.storageAccess.inspect(prove, (lease) =>
      inspectPool(this.inventory.pool, lease),
    );
    verifyPoolIdentity(this.inventory.pool, current);
    if (!this.host) {
      current.fds = await deviceFds(
        [current.data, current.metadata, await deviceNode(current.path)],
        await pids(),
        [current.data, current.metadata],
      );
      admitPool(current);
    } else {
      // ThinPoolDisk::open removes inactive orphans even on restart.
      // Reject any inactive id before the replacement host can open the pool.
      check(
        current.thinIds.every((id) =>
          current.mappings.some((m) => m.id === id),
        ) && current.thinIds.length === current.mappings.length,
        "fc_restart_inactive_thin",
      );
      await this.validateMappings(current);
    }
    // The engine releases the host's start barrier only after this hook returns.
    this.storageAccess.start();
  }
  private async validateMappings(pool: PoolInventory) {
    for (const m of pool.mappings) {
      check(m.name.startsWith(pool.name + "."), "fc_foreign_thin_name");
      const volume = m.name.slice(pool.name.length + 1);
      const sandbox =
        /^(?:tmp-)?sb-([a-f0-9]{32})-(?:root|swap)(?:-pre|-ckpt-[a-f0-9]{32}-v[0-9]+)?$/.exec(
          volume,
        );
      const image = /^(?:tmp-)?img-[a-f0-9]{32}-v[0-9]+$/.test(volume);
      check(
        image || (sandbox && this.ownedSandboxes.has(sandbox[1]!)),
        "fc_foreign_thin_owner",
      );
    }
  }
  async qualifyKernel(pid: number) {
    verifyPoolIdentity(
      this.inventory.pool,
      await inspectPool(this.inventory.pool),
    );
    const mounts = verifyFcKernelPosture(
      await readFile(`/proc/${pid}/mountinfo`, "utf8"),
      await readFile(`/proc/${pid}/status`, "utf8"),
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
    const nodeMount = mounts.find((fields) => fields[4] === "/dev/engram");
    check(
      nodeMount &&
        nodeMount[5]?.split(",").includes("rw") &&
        !nodeMount[5]?.split(",").includes("nodev"),
      "fc_host_nodes_mount",
    );
    const pool = this.inventory.pool;
    for (const expected of [
      pool.control,
      pool.metadata,
      await deviceNode(pool.path),
    ]) {
      const projected = await lstat(`/proc/${pid}/root${expected.path}`, {
        bigint: true,
      });
      check(
        String(projected.rdev) === expected.rdev &&
          (expected.path === pool.control.path
            ? projected.isCharacterDevice()
            : projected.isBlockDevice()),
        "fc_host_device_projection",
      );
    }
    const nodes = await lstat(`/proc/${pid}/root/dev/engram`, { bigint: true });
    const source = await lstat(join(this.root, "thin-nodes"), { bigint: true });
    check(
      nodes.isDirectory() &&
        nodes.dev === source.dev &&
        nodes.ino === source.ino,
      "fc_host_nodes_projection",
    );
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
    if (this.storageAccess.inspectionActive) {
      await this.observation.catch(() => {});
      check(!this.evidenceWriteFailed, "fc_physical_evidence_write");
      return;
    }
    const previous = this.observation;
    const current = previous.catch(() => {}).then(() => this.observeOnce());
    this.observation = current;
    return current;
  }
  private async observeOnce() {
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
    const pool = await inspectPool(this.inventory.pool);
    verifyPoolIdentity(this.inventory.pool, pool);
    const nodes = await Promise.all(
      pool.mappings.map(async (m) => {
        const name = /^DEVNAME=(dm-[0-9]+)$/m.exec(
          await readFile(`/sys/dev/block/${m.device}/uevent`, "utf8"),
        )?.[1];
        check(name, "fc_thin_device_name");
        return deviceNode(`/dev/${name}`);
      }),
    );
    const fds = await deviceFds(nodes, all);
    check(
      fds.every((fd) => identities.has(fd.pid)),
      "fc_thin_foreign_fd",
    );
    const seenProcesses = new Set<number>();
    const work = join(this.root, "host");
    const candidates: BootCandidate[] = [];
    for (const name of (await readdir(work)).filter((v) => uuid.test(v))) {
      let raw: string;
      try {
        raw = (
          await readOwned(join(work, name, "sandbox.json"), 1024 * 1024, false)
        ).toString();
      } catch (error) {
        if (absent(error)) continue;
        throw error;
      }
      const manifest = JSON.parse(raw);
      check(
        manifest.schema_version === 3 &&
          manifest.backend === "firecracker" &&
          manifest.sandbox_id === name,
        "fc_sandbox_manifest",
      );
      const fc = manifest.firecracker;
      const process = identities.get(fc?.pid);
      if (!process) continue;
      check(
        process.birth === String(fc.start_time_jiffies),
        "fc_sandbox_process",
      );
      check(
        typeof fc.rootfs_canonical === "string" &&
          dirname(fc.rootfs_canonical) === join(work, "rootfs") &&
          /^[a-f0-9-]{36}\.dev$/.test(basename(fc.rootfs_canonical)),
        "fc_rootfs_path",
      );
      candidates.push({
        name,
        process,
        rootfsCanonical: fc.rootfs_canonical,
        sandboxManifest: raw,
      });
    }
    const commands: { process: FcProcess; args: string[] }[] = [];
    for (const process of identities.values()) {
      if (process.executable !== "/usr/local/bin/firecracker") continue;
      try {
        commands.push({ process, args: await processArguments(process.pid) });
      } catch (error) {
        if (!absent(error)) throw error;
      }
    }
    const records = await readdir(join(work, "managed-boots"));
    check(records.length <= 8192, "fc_managed_record_bound");
    for (const file of records.filter((v) => /^[a-f0-9-]{36}\.boot$/.test(v))) {
      const name = file.slice(0, -5);
      const bootBytes = await readOwned(
        join(work, "managed-boots", file),
        16 * 1024 * 1024,
      );
      const boot = parseHostRecord(bootBytes, name).record;
      check(
        boot.request.key.context.target.host_id ===
          (await readOwned(join(work, "host_id"))).toString(),
        "fc_managed_host_identity",
      );
      // A verified durable request owns staging/transient volumes even before
      // its Firecracker process is started.
      this.ownedSandboxes.add(name.replaceAll("-", ""));
      const paths = managedBootPaths(work, name, boot.request.key.boot_attempt);
      if (managedNeedsIo(boot))
        this.requiredBoots.add(`${name}:${boot.request.key.boot_attempt}`);
      const process = selectManagedProcess(commands, paths.apiSocket);
      if (!process) continue;
      candidates.push({
        name,
        process,
        rootfsCanonical: paths.rootfsCanonical,
        apiSocket: paths.apiSocket,
        boot,
        bootBytes,
      });
    }
    for (const candidate of candidates) {
      const { name, process, rootfsCanonical, boot, bootBytes, apiSocket } =
        candidate;
      check(
        this.inputs.firecracker &&
          process.sha256 === this.inputs.firecracker.sha256 &&
          process.uid === 0 &&
          process.executable === "/usr/local/bin/firecracker",
        "fc_sandbox_process",
      );
      check(!seenProcesses.has(process.pid), "fc_boot_process_duplicate");
      // The symlink target exists inside the trusted host's device mount.
      check(
        (await lstat(rootfsCanonical)).isSymbolicLink(),
        "fc_rootfs_not_thin",
      );
      const target = await readlink(rootfsCanonical);
      const volume = `sb-${name.replaceAll("-", "")}-root`;
      const expectedName = `${pool.name}.${volume}`;
      check(target === `/dev/engram/${expectedName}`, "fc_rootfs_volume_name");
      const mapping = pool.mappings.find((m) => m.name === expectedName);
      check(mapping, "fc_rootfs_not_thin");
      const node = await deviceNode(
        join(this.root, "thin-nodes", expectedName),
      );
      check(node.device === mapping.device, "fc_rootfs_device_changed");
      seenProcesses.add(process.pid);
      this.ownedSandboxes.add(name.replaceAll("-", ""));
      const rootBytes = BigInt(mapping.table.split(" ")[1]!) * 512n;
      if (boot)
        check(
          rootBytes === BigInt(boot.request.cold.disk.size_bytes),
          "fc_boot_volume_size",
        );
      const largest = (pattern: RegExp) =>
        pool.mappings
          .filter((m) => pattern.test(m.name))
          .map((m) => BigInt(m.table.split(" ")[1]!) * 512n)
          .reduce((a, b) => (a > b ? a : b), 0n);
      requirePoolBudget(
        pool,
        rootBytes,
        largest(/-swap(?:-|$)/),
        largest(/\.(?:tmp-)?img-/),
      );
      const identity = JSON.stringify({
        mapping: {
          name: mapping.name,
          device: mapping.device,
          table: mapping.table,
          id: mapping.id,
        },
        node,
        process,
      });
      const bootAttempt =
        boot?.request.key.boot_attempt ?? `ordinary-${process.birth}`;
      const key = `${name}:${bootAttempt}`;
      const needsIo = !boot || managedNeedsIo(boot);
      if (needsIo) this.requiredBoots.add(key);
      const prior = this.bootDevices.get(key);
      verifyBootDevice(prior, identity);
      this.bootDevices.set(key, identity);
      const io = (await readFile(`/sys/dev/block/${node.device}/stat`, "utf8"))
        .trim()
        .split(/\s+/);
      check(io.length >= 11 && /^[0-9]+$/.test(io[2]!), "fc_thin_io_counter");
      const deviceFd = fds.find(
        (fd) => fd.pid === process.pid && fd.rdev === node.rdev,
      );
      check(deviceFd, "fc_rootfs_device_fd");
      // A newly allocated thin device starts counters at zero. Bind the
      // counters to this boot's exact table, thin id and process birth.
      const readSectorsBefore = this.bootReads.get(key);
      if (readSectorsBefore === undefined) {
        this.bootReads.set(key, BigInt(io[2]!));
        continue;
      }
      if (!needsIo || BigInt(io[2]!) <= readSectorsBefore) continue;
      const finalFd = await stat(`/proc/${process.pid}/fd/${deviceFd.fd}`, {
        bigint: true,
      });
      check(
        finalFd.isBlockDevice() && String(finalFd.rdev) === node.rdev,
        "fc_rootfs_device_fd_changed",
      );
      verifyFcProcessIdentity(
        process,
        await processIdentity(process.pid, this.inventory.bootId, true),
      );
      if (apiSocket) {
        const finalArgs = await processArguments(process.pid);
        check(
          selectManagedProcess([{ process, args: finalArgs }], apiSocket),
          "fc_managed_process_arguments_changed",
        );
      }
      if (this.observations.has(key)) continue;
      await this.retainObservation(
        key,
        JSON.stringify({
          sandboxId: name,
          ...(boot ? { sessionId: boot.request.cold.session_id } : {}),
          bootAttempt,
          kind: boot ? "managed" : "ordinary",
          process,
          pool: { name: pool.name, device: pool.device, table: pool.table },
          mapping,
          deviceFd,
          node,
          volume,
          readSectorsBefore: String(readSectorsBefore),
          readSectors: io[2],
          rootfsCanonical,
          ...(apiSocket ? { apiSocket } : {}),
          ...(bootBytes ? { bootRecordSha256: hash(bootBytes) } : {}),
          ...(candidate.sandboxManifest
            ? { sandboxManifestSha256: hash(candidate.sandboxManifest) }
            : {}),
        }) + "\n",
      );
      this.observationKinds.add(boot ? "managed" : "ordinary");
    }
    check(
      [...identities.values()]
        .filter((p) => p.executable === "/usr/local/bin/firecracker")
        .every((p) => seenProcesses.has(p.pid)),
      "fc_unobserved_process",
    );
    await this.validateMappings(pool);
    for (const name of this.retainedVolumes.keys())
      if (!pool.mappings.some((m) => m.name === name))
        this.retainedVolumes.delete(name);
    for (const mapping of pool.mappings) {
      if (/\.(?:img-|sb-.*-ckpt-)/.test(mapping.name)) {
        const previous = this.retainedVolumes.get(mapping.name);
        if (previous) {
          const prior = JSON.parse(previous);
          check(
            prior.id === mapping.id &&
              prior.device === mapping.device &&
              prior.table === mapping.table,
            "fc_retained_volume_replaced",
          );
        }
        this.retainedVolumes.set(mapping.name, JSON.stringify(mapping));
      }
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
    for (const key of this.requiredBoots)
      check(this.observations.has(key), "fc_physical_boot_unobserved");
    const files = await readdir(join(this.root, "host/managed-boots"));
    const boots = files.filter((v) => /^[a-f0-9-]{36}\.boot$/.test(v));
    check(boots.length > 0 && boots.length <= 4096, "fc_physical_boot_count");
    for (const file of boots) {
      const id = file.slice(0, -5);
      const boot = parseHostRecord(
        await readOwned(join(this.root, "host/managed-boots", file)),
        id,
      ).record;
      requireManagedBootCoverage(
        this.requiredBoots,
        this.observations,
        `${id}:${boot.request.key.boot_attempt}`,
        boot,
      );
    }
  }
  async requireInfrastructure(lifecycle: {
    stop(): Promise<void>;
    restart(): Promise<void>;
  }) {
    await this.observe();
    check(
      this.observationKinds.has("ordinary") &&
        this.observationKinds.has("managed"),
      "fc_infrastructure_boot_evidence",
    );
    await this.requireCoverage();
    const deadline = performance.now() + 10000;
    for (;;) {
      try {
        await this.requireReleased();
        break;
      } catch (error) {
        if (performance.now() >= deadline) throw error;
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
      await this.observe();
    }
    // The live checks above require normal deletion while the host is alive.
    // Metadata inspection starts only after that exact host and its VMs stop.
    await lifecycle.stop();
    await this.requireStopped();
    await lifecycle.restart();
    await put(
      join(this.root, "fc-infrastructure.json"),
      JSON.stringify({
        bootKeys: [...this.observations],
        pool: this.inventory.pool,
        release: "verified",
      }),
    );
  }
  async requireReleased() {
    const pool = await inspectPool(this.inventory.pool);
    await this.checkReleased(pool, false);
  }
  private async checkReleased(pool: PoolInventory, metadata: boolean) {
    verifyPoolIdentity(this.inventory.pool, pool);
    await this.validateMappings(pool);
    verifyReleasedMappings(pool, this.retainedVolumes);
    for (const m of pool.mappings) {
      const path = join(this.root, "thin-nodes", m.name);
      const node = await deviceNode(path);
      check(
        node.device === m.device &&
          (await readdir(`/sys/dev/block/${m.device}/holders`)).length === 0,
        "fc_thin_remaining_holder",
      );
      check(
        (await deviceFds([node], await pids())).length === 0,
        "fc_thin_remaining_fd",
      );
    }
    const nodeNames = (
      await readdir(join(this.root, "thin-nodes")).catch((error) => {
        if (!this.host && absent(error)) return [];
        throw error;
      })
    ).sort();
    check(
      JSON.stringify(nodeNames) ===
        JSON.stringify(pool.mappings.map((m) => m.name).sort()),
      "fc_unreleased_device_node",
    );
    await put(
      join(
        this.root,
        `fc-volume-cleanup-${randomBytes(8).toString("hex")}.json`,
      ),
      JSON.stringify({
        metadataInspected: metadata,
        pool,
        retained: pool.mappings.map((m) => m.name),
      }),
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
    const prove = () => this.proveStorageQuiescent();
    await this.storageAccess.stopped(prove);
    await this.storageAccess.inspect(prove, async (lease) => {
      const pool = await inspectPool(this.inventory.pool, lease);
      verifyThinIds(pool);
      await this.checkReleased(pool, true);
    });
  }
}
