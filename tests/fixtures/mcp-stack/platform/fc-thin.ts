import { lstat, readFile, readdir, realpath, statfs } from "node:fs/promises";
import { lstatSync, realpathSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { check, command, childEnvironment, record } from "./owned.ts";

export type ThinPoolInput = { name: string; directory: string };
export function parseThinPool(value: unknown): ThinPoolInput {
  const o = record(value, ["name", "directory"]);
  check(
    typeof o.name === "string" &&
      /^[A-Za-z][A-Za-z0-9_.-]{0,126}$/.test(o.name),
    "fc_pool_name",
  );
  check(
    typeof o.directory === "string" &&
      o.directory.startsWith("/") &&
      resolve(o.directory) === o.directory,
    "fc_pool_directory",
  );
  for (let p = o.directory; ; p = dirname(p)) {
    check(!lstatSync(p).isSymbolicLink(), "fc_pool_directory_symlink");
    if (p === "/") break;
  }
  const s = lstatSync(o.directory);
  check(
    s.isDirectory() &&
      s.uid === process.getuid?.() &&
      (s.mode & 0o777) === 0o700 &&
      realpathSync(o.directory) === o.directory,
    "fc_pool_directory_owner_mode",
  );
  return { name: o.name, directory: o.directory };
}
export type DeviceNode = {
  path: string;
  device: string;
  ino: string;
  rdev: string;
};
export type Backing = DeviceNode & {
  file: string;
  fileDev: string;
  fileIno: string;
  bytes: string;
  allocated: string;
};
export type ThinMapping = {
  name: string;
  device: string;
  table: string;
  id: number;
  open: number;
};
export type PoolInventory = ThinPoolInput & {
  device: string;
  path: string;
  table: string;
  major: string;
  node: DeviceNode;
  otherTables: string;
  metadata: Backing;
  data: Backing;
  control: DeviceNode;
  status: string;
  thinIds: number[];
  mappings: ThinMapping[];
  holders: string[];
  fds: { pid: number; fd: string; rdev: string }[];
};
export function linuxDevice(value: bigint): string {
  return `${((value >> 8n) & 0xfffn) | ((value >> 32n) & 0xfffff000n)}:${(value & 0xffn) | ((value >> 12n) & 0xffffff00n)}`;
}
export function parsePoolTable(table: string) {
  const m =
    /^0 ([1-9][0-9]*) thin-pool ([0-9]+:[0-9]+) ([0-9]+:[0-9]+) ([1-9][0-9]*) ([0-9]+) 0$/.exec(
      table,
    );
  check(m, "fc_pool_table");
  const blockSectors = BigInt(m[4]!);
  check(blockSectors === 128n && m[2] !== m[3], "fc_pool_block_size");
  return { sectors: BigInt(m[1]!), metadata: m[2]!, data: m[3]!, blockSectors };
}
export function parsePoolStatus(
  status: string,
  blockSectors: bigint,
  snapshot = false,
) {
  const m =
    /^0 ([1-9][0-9]*) thin-pool [0-9]+ ([0-9]+)\/([1-9][0-9]*) ([0-9]+)\/([1-9][0-9]*) (-|[0-9]+) rw (?:discard_passdown|no_discard_passdown) (?:queue_if_no_space|error_if_no_space) - ([0-9]+)$/.exec(
      status,
    );
  check(m && (snapshot || m[6] === "-"), "fc_pool_health");
  const metaUsed = BigInt(m[2]!),
    metaTotal = BigInt(m[3]!);
  const dataUsed = BigInt(m[4]!),
    dataTotal = BigInt(m[5]!);
  check(
    metaUsed + BigInt(m[7]!) < metaTotal && dataUsed < dataTotal,
    "fc_pool_capacity",
  );
  return {
    dataFree: (dataTotal - dataUsed) * blockSectors * 512n,
    dataTotal: dataTotal * blockSectors * 512n,
    metadataFree: (metaTotal - metaUsed) * 4096n,
    metadataTotal: metaTotal * 4096n,
  };
}
// Kernel guidance: 48 metadata bytes per data block, plus 25% reserve.
// https://docs.kernel.org/admin-guide/device-mapper/thin-provisioning.html
const metadataBudget = (bytes: bigint, blockSectors: bigint) =>
  (((bytes + blockSectors * 512n - 1n) / (blockSectors * 512n)) * 48n * 5n +
    3n) /
  4n;
export function admitPool(pool: PoolInventory) {
  const table = parsePoolTable(pool.table);
  const capacity = parsePoolStatus(pool.status, table.blockSectors);
  check(
    pool.mappings.length === 0 &&
      pool.thinIds.length === 0 &&
      pool.holders.length === 0 &&
      pool.fds.length === 0,
    "fc_pool_not_empty",
  );
  check(
    capacity.dataFree * 5n >= capacity.dataTotal &&
      capacity.metadataFree >=
        metadataBudget(capacity.dataTotal, table.blockSectors) &&
      capacity.metadataFree * 10n >= capacity.metadataTotal,
    "fc_pool_capacity",
  );
  check(
    table.sectors * 512n === BigInt(pool.data.bytes) &&
      capacity.dataTotal <= BigInt(pool.data.bytes),
    "fc_pool_backing_size",
  );
  for (const backing of [pool.data, pool.metadata])
    check(
      BigInt(backing.allocated) >= BigInt(backing.bytes),
      "fc_pool_sparse_backing",
    );
}
export function verifyPoolIdentity(
  expected: PoolInventory,
  actual: PoolInventory,
) {
  const identity = (p: PoolInventory) => ({
    name: p.name,
    directory: p.directory,
    device: p.device,
    path: p.path,
    table: p.table,
    major: p.major,
    metadata: p.metadata,
    data: p.data,
    control: p.control,
    node: p.node,
    otherTables: p.otherTables,
  });
  check(
    JSON.stringify(identity(expected)) === JSON.stringify(identity(actual)),
    "fc_pool_identity_changed",
  );
}
export async function dm(args: string[]) {
  return (await command("/usr/sbin/dmsetup", args, "/", childEnvironment("/")))
    .toString()
    .trim();
}
export async function deviceNode(
  path: string,
  character = false,
): Promise<DeviceNode> {
  const s = await lstat(path, { bigint: true });
  check(
    (character ? s.isCharacterDevice() : s.isBlockDevice()) &&
      s.uid === 0n &&
      !s.isSymbolicLink() &&
      (await realpath(path)) === path,
    "fc_device_node",
  );
  return {
    path,
    device: linuxDevice(s.rdev),
    ino: String(s.ino),
    rdev: String(s.rdev),
  };
}
async function backing(device: string, file: string): Promise<Backing> {
  const sys = `/sys/dev/block/${device}`;
  const uevent = await readFile(join(sys, "uevent"), "utf8");
  const name = /^DEVNAME=(loop[0-9]+)$/m.exec(uevent)?.[1];
  check(name, "fc_pool_loop_device");
  const node = await deviceNode(`/dev/${name}`);
  const linked = (
    await readFile(join(sys, "loop/backing_file"), "utf8")
  ).trim();
  check(
    node.device === device &&
      linked === file &&
      (await realpath(file)) === file,
    "fc_pool_backing_path",
  );
  const s = await lstat(file, { bigint: true });
  check(
    s.isFile() &&
      s.nlink === 1n &&
      s.uid === BigInt(process.getuid?.() ?? -1) &&
      (s.mode & 0o022n) === 0n,
    "fc_pool_backing_owner",
  );
  // losetup reads the kernel's loop_info64, including backing inode/device.
  const loops = JSON.parse(
    (
      await command(
        "/usr/sbin/losetup",
        [
          "--json",
          "--output",
          "NAME,BACK-FILE,BACK-INO,BACK-MAJ:MIN,OFFSET,SIZELIMIT",
          node.path,
        ],
        "/",
        childEnvironment("/"),
      )
    ).toString(),
  );
  const loop = loops.loopdevices?.[0];
  check(
    loops.loopdevices?.length === 1 &&
      loop.name === node.path &&
      loop["back-file"] === file &&
      String(loop["back-ino"]) === String(s.ino) &&
      loop["back-maj:min"] === linuxDevice(s.dev) &&
      Number(loop.offset) === 0 &&
      Number(loop.sizelimit) === 0,
    "fc_pool_backing_identity",
  );
  return {
    ...node,
    file,
    fileDev: String(s.dev),
    fileIno: String(s.ino),
    bytes: String(s.size),
    allocated: String(s.blocks * 512n),
  };
}
export async function poolMappings(poolDevice: string): Promise<ThinMapping[]> {
  const raw = await dm([
    "info",
    "-c",
    "--noheadings",
    "--separator",
    "|",
    "-o",
    "name,major,minor,open",
  ]);
  const found: ThinMapping[] = [];
  for (const line of raw.split("\n").filter(Boolean)) {
    const [name, major, minor, opened, ...extra] = line
      .trim()
      .split("|")
      .map((v) => v.trim());
    check(
      name &&
        /^[A-Za-z0-9_.+-]+$/.test(name) &&
        /^[0-9]+$/.test(major!) &&
        /^[0-9]+$/.test(minor!) &&
        /^[0-9]+$/.test(opened!) &&
        extra.length === 0,
      "fc_dm_inventory",
    );
    const table = await dm(["table", name]);
    if (!table.includes(" thin ")) continue;
    const m = /^0 ([1-9][0-9]*) thin ([0-9]+:[0-9]+) ([0-9]+)$/.exec(table);
    check(m, "fc_thin_table");
    if (m[2] === poolDevice)
      found.push({
        name,
        device: `${major}:${minor}`,
        table,
        id: Number(m[3]),
        open: Number(opened),
      });
  }
  return found.sort((a, b) => a.name.localeCompare(b.name));
}
const offlineLease = Symbol("offline-pool-inspection");
export type OfflinePoolLease = { readonly [offlineLease]: () => boolean };

/** The fixture blocks every host start while an offline pool read owns this gate.
 * Stopped state is granted only after the caller proves kernel quiescence. */
export class OfflinePoolAccess {
  private hostMayRun = false;
  private inspecting = false;
  get inspectionActive() {
    return this.inspecting;
  }
  start() {
    check(
      !this.inspecting && !this.hostMayRun,
      "fc_metadata_host_start_blocked",
    );
    this.hostMayRun = true;
  }
  async stopped(prove: () => Promise<void>) {
    check(!this.inspecting, "fc_pool_inspection_busy");
    this.inspecting = true;
    try {
      await prove();
      this.hostMayRun = false;
    } finally {
      this.inspecting = false;
    }
  }
  async inspect<T>(
    prove: () => Promise<void>,
    read: (lease: OfflinePoolLease) => Promise<T>,
  ): Promise<T> {
    check(!this.hostMayRun && !this.inspecting, "fc_pool_host_not_quiescent");
    this.inspecting = true;
    let held = true;
    const lease = {
      [offlineLease]: () => held && this.inspecting && !this.hostMayRun,
    };
    try {
      await prove();
      return await read(lease);
    } finally {
      held = false;
      this.inspecting = false;
    }
  }
}

export async function inspectPool(
  input: ThinPoolInput,
  lease?: OfflinePoolLease,
): Promise<PoolInventory> {
  input = parseThinPool({ name: input.name, directory: input.directory });
  check(input.name.length <= 26, "fc_pool_name_volume_budget");
  const table = await dm(["table", input.name]);
  const parsed = parsePoolTable(table);
  const number = await dm([
    "info",
    "-c",
    "--noheadings",
    "--separator",
    ":",
    "-o",
    "major,minor",
    input.name,
  ]);
  check(/^[0-9]+:[0-9]+$/.test(number), "fc_pool_device");
  const sys = `/sys/dev/block/${number}`;
  const devname = /^DEVNAME=(dm-[0-9]+)$/m.exec(
    await readFile(join(sys, "uevent"), "utf8"),
  )?.[1];
  check(
    devname &&
      (await readFile(join(sys, "dm/name"), "utf8")).trim() === input.name,
    "fc_pool_name_identity",
  );
  const node = await deviceNode(`/dev/${devname}`);
  check(node.device === number, "fc_pool_device_identity");
  const metadata = await backing(
    parsed.metadata,
    join(input.directory, "metadata.img"),
  );
  const data = await backing(parsed.data, join(input.directory, "data.img"));
  const status = await dm(["status", input.name]);
  parsePoolStatus(status, parsed.blockSectors, !lease);
  const thinIds = lease
    ? await readThinIds(input.name, metadata.path, status, lease)
    : [];
  const filesystem = await statfs(input.directory, { bigint: true });
  check(
    filesystem.bavail * filesystem.bsize >= 16n * 1024n ** 3n,
    "fc_pool_filesystem_reserve",
  );
  const mappings = await poolMappings(number);
  const holders = await readdir(join(sys, "holders"));
  for (const holder of holders) {
    const device = (
      await readFile(join(sys, "holders", holder, "dev"), "utf8")
    ).trim();
    check(
      mappings.some((m) => m.device === device),
      "fc_foreign_pool_holder",
    );
  }
  const ownNames = new Set([input.name, ...mappings.map((m) => m.name)]);
  const tables = await dm(["table"]);
  const otherTables = tables
    .split("\n")
    .filter((line) => !ownNames.has(line.split(": ")[0]!))
    .sort()
    .join("\n");
  check(!/ thin(?:-pool)? /.test(otherTables), "fc_foreign_thin_pool");
  return {
    ...input,
    device: number,
    path: node.path,
    table,
    major: number.split(":")[0]!,
    node,
    otherTables,
    metadata,
    data,
    control: await deviceNode("/dev/mapper/control", true),
    status,
    thinIds,
    mappings,
    holders,
    fds: [],
  };
}

export type MetadataReadState = { pending: Promise<void>; failed: boolean };
const metadataRead: MetadataReadState = {
  pending: Promise.resolve(),
  failed: false,
};
export async function readThinIds(
  name: string,
  path: string,
  status: string,
  lease: OfflinePoolLease,
  control = dm,
  dump = async () =>
    (
      await command(
        "/usr/sbin/thin_dump",
        ["--skip-mappings", "-m", path],
        "/",
        childEnvironment("/"),
      )
    ).toString(),
  state: MetadataReadState = metadataRead,
): Promise<number[]> {
  const previous = state.pending;
  let release!: () => void;
  state.pending = new Promise<void>((resolve) => {
    release = resolve;
  });
  await previous;
  try {
    check(lease[offlineLease](), "fc_pool_host_not_quiescent");
    check(!state.failed, "fc_pool_metadata_release_uncertain");
    parsePoolStatus(status, 128n);
    await control(["message", name, "0", "reserve_metadata_snap"]);
    try {
      const xml = await dump();
      check(
        xml.includes("<superblock") && xml.includes("</superblock>"),
        "fc_pool_metadata_dump",
      );
      const ids = [...xml.matchAll(/<device\s+dev_id="([0-9]+)"/g)].map((m) =>
        Number(m[1]),
      );
      check(
        (xml.match(/<device\b/g) ?? []).length === ids.length &&
          new Set(ids).size === ids.length &&
          ids.every((id) => Number.isSafeInteger(id) && id <= 0xffffff),
        "fc_pool_metadata_ids",
      );
      return ids;
    } finally {
      try {
        await control(["message", name, "0", "release_metadata_snap"]);
      } catch (error) {
        state.failed = true;
        throw error;
      }
    }
  } finally {
    release();
  }
}

export function requirePoolBudget(
  pool: PoolInventory,
  rootBytes: bigint,
  swapBytes: bigint,
  imageBytes = rootBytes,
) {
  const capacity = parsePoolStatus(
    pool.status,
    parsePoolTable(pool.table).blockSectors,
    true,
  );
  // One staged image plus two live, two checkpoint, and two in-flight root
  // volumes; each swap has the same overlap, without a staged image.
  const overlap = imageBytes + rootBytes * 6n + swapBytes * 6n;
  const reserve = (overlap + 3n) / 4n;
  check(
    capacity.dataTotal >= overlap + reserve &&
      capacity.dataFree >= reserve &&
      capacity.metadataFree >=
        metadataBudget(
          overlap + reserve,
          parsePoolTable(pool.table).blockSectors,
        ),
    "fc_pool_overlap_capacity",
  );
}
export function verifyBootDevice(
  previous: string | undefined,
  current: string,
) {
  check(
    previous === undefined || previous === current,
    "fc_boot_device_changed",
  );
}

export function verifyThinIds(pool: PoolInventory) {
  check(
    pool.thinIds.length === pool.mappings.length &&
      pool.thinIds.every((id) => pool.mappings.some((m) => m.id === id)),
    "fc_unexplained_inactive_thin",
  );
}
export function verifyReleasedMappings(
  pool: PoolInventory,
  retained: ReadonlyMap<string, string>,
) {
  for (const m of pool.mappings) {
    const previous = retained.get(m.name);
    const volume = m.name.slice(pool.name.length + 1);
    check(
      previous &&
        m.name.startsWith(pool.name + ".") &&
        /^(?:img-[a-f0-9]{32}-v[0-9]+|sb-[a-f0-9]{32}-(?:root|swap)-ckpt-[a-f0-9]{32}-v[0-9]+)$/.test(
          volume,
        ),
      "fc_unreleased_volume",
    );
    const expected = JSON.parse(previous);
    check(
      m.id === expected.id &&
        m.device === expected.device &&
        m.table === expected.table &&
        m.open === 0,
      "fc_retained_volume_changed_or_busy",
    );
  }
}
