import { expect, test } from "bun:test";
import {
  admitPool,
  OfflinePoolAccess,
  readThinIds as reservedThinIds,
  verifyThinIds,
  type MetadataReadState,
  type OfflinePoolLease,
  verifyPoolIdentity,
  parsePoolTable,
  parsePoolStatus,
} from "./fc-thin.ts";
import { poolFixture } from "./fc-thin-fixture.ts";
function readThinIds(
  name: string,
  path: string,
  status: string,
  control?: (args: string[]) => Promise<string>,
  dump?: () => Promise<string>,
  state?: MetadataReadState,
) {
  return new OfflinePoolAccess().inspect(
    async () => {},
    (lease) => reservedThinIds(name, path, status, lease, control, dump, state),
  );
}
test("thin admission refuses foreign active or inactive volumes, holders and unhealthy capacity", () => {
  const good = poolFixture();
  expect(() => admitPool(good)).not.toThrow();
  for (const patch of [
    { thinIds: [1] },
    { holders: ["dm-1"] },
    { fds: [{ pid: 42, fd: "3", rdev: "1" }] },
    {
      mappings: [
        {
          name: "foreign",
          device: "253:1",
          table: "0 8 thin 253:0 1",
          id: 1,
          open: 1,
        },
      ],
    },
    { status: good.status.replace(" rw ", " ro ") },
    { status: good.status.replace("0/2097152", "2097151/2097152") },
    { status: good.status.replace("10/262144", "262143/262144") },
    { data: { ...good.data, allocated: "1" } },
  ])
    expect(() => admitPool({ ...good, ...patch })).toThrow();
});
test("pool identity binds table, backing inode and actual device across launch", () => {
  const good = poolFixture();
  expect(() => verifyPoolIdentity(good, good)).not.toThrow();
  for (const patch of [
    { device: "253:1" },
    { table: good.table.replace("7:0", "7:2") },
    { data: { ...good.data, fileIno: "13" } },
  ])
    expect(() => verifyPoolIdentity(good, { ...good, ...patch })).toThrow(
      "fc_pool_identity_changed",
    );
});
test("thin table and status reject alternate targets, features and unhealthy metadata", () => {
  expect(parsePoolTable(poolFixture().table).blockSectors).toBe(128n);
  expect(() => parsePoolTable("0 10 linear 7:0 0")).toThrow();
  expect(() =>
    parsePoolStatus(poolFixture().status.replace(" - rw", " 2 rw"), 128n),
  ).toThrow();
});

test("metadata inspection refuses a foreign reservation and releases only its own snapshot", async () => {
  for (const failure of ["none", "dump", "release", "reserve"] as const) {
    const calls: string[] = [];
    const operation = readThinIds(
      "suite",
      "/dev/loop0",
      poolFixture().status,
      async (args) => {
        const last = args.at(-1)!;
        calls.push(last);
        if (last === `${failure}_metadata_snap`) throw Error(failure);
        return "";
      },
      async () => {
        calls.push("dump");
        if (failure === "dump") throw Error("dump");
        return '<superblock><device dev_id="7"/></superblock>';
      },
      { pending: Promise.resolve(), failed: false },
    );
    if (failure === "none") expect(await operation).toEqual([7]);
    else await expect(operation).rejects.toThrow(failure);
    expect(calls).toEqual(
      failure === "reserve"
        ? ["reserve_metadata_snap"]
        : ["reserve_metadata_snap", "dump", "release_metadata_snap"],
    );
  }
  let invoked = false;
  await expect(
    readThinIds(
      "suite",
      "/dev/loop0",
      poolFixture().status.replace(" - rw", " 9 rw"),
      async () => {
        invoked = true;
        return "";
      },
      async () => "",
    ),
  ).rejects.toThrow("fc_pool_health");
  expect(invoked).toBe(false);
});

test("a release timeout poisons later inspections and never releases another reservation", async () => {
  const state = { pending: Promise.resolve(), failed: false };
  const calls: string[] = [];
  const control = async (args: string[]) => {
    const op = args.at(-1)!;
    calls.push(op);
    if (op === "release_metadata_snap") throw Error("release timeout");
    return "";
  };
  await expect(
    readThinIds(
      "suite",
      "/dev/loop0",
      poolFixture().status,
      control,
      async () => "<superblock></superblock>",
      state,
    ),
  ).rejects.toThrow("release timeout");
  await expect(
    readThinIds(
      "suite",
      "/dev/loop0",
      poolFixture().status,
      control,
      async () => "",
      state,
    ),
  ).rejects.toThrow("fc_pool_metadata_release_uncertain");
  expect(calls).toEqual(["reserve_metadata_snap", "release_metadata_snap"]);
});
test("live status permits the trusted host's metadata reservation without changing it", () => {
  const reserved = poolFixture().status.replace(" - rw", " 9 rw");
  expect(() => parsePoolStatus(reserved, 128n, true)).not.toThrow();
  expect(() => parsePoolStatus(reserved, 128n, false)).toThrow(
    "fc_pool_health",
  );
});

test("capacity charges actual installed image/session/swap overlap rather than case count", async () => {
  const { requirePoolBudget } = await import("./fc-thin.ts");
  expect(() =>
    requirePoolBudget(poolFixture(), 8n * 1024n ** 3n, 2n * 1024n ** 3n),
  ).not.toThrow();
  expect(() =>
    requirePoolBudget(poolFixture(), 64n * 1024n ** 3n, 2n * 1024n ** 3n),
  ).toThrow("fc_pool_overlap_capacity");
});
test("per-boot evidence rejects reused minors, thin IDs, volume names and stale process birth", async () => {
  const { verifyBootDevice } = await import("./fc-thin.ts");
  const before = {
    device: "253:1",
    thin: 7,
    volume: "sb-owned-root",
    birth: "100",
  };
  expect(() =>
    verifyBootDevice(undefined, JSON.stringify(before)),
  ).not.toThrow();
  expect(() =>
    verifyBootDevice(JSON.stringify(before), JSON.stringify(before)),
  ).not.toThrow();
  for (const patch of [
    { device: "253:2" },
    { thin: 8 },
    { volume: "sb-other-root" },
    { birth: "101" },
  ])
    expect(() =>
      verifyBootDevice(
        JSON.stringify(before),
        JSON.stringify({ ...before, ...patch }),
      ),
    ).toThrow("fc_boot_device_changed");
});

test("kernel status includes the metadata low-water mark and refuses pressure", () => {
  const status = poolFixture().status;
  expect(() => parsePoolStatus(status, 128n)).not.toThrow();
  expect(() =>
    parsePoolStatus(status.replace(" 1024", " 262144"), 128n),
  ).toThrow("fc_pool_capacity");
});

test("release keeps only recorded closed image/checkpoint volumes with exact identity", async () => {
  const { verifyReleasedMappings } = await import("./fc-thin.ts");
  const mapping = {
    name: "suite.img-" + "a".repeat(32) + "-v1",
    device: "253:1",
    table: "0 16 thin 253:0 7",
    id: 7,
    open: 0,
  };
  const retained = new Map([[mapping.name, JSON.stringify(mapping)]]);
  const pool = { ...poolFixture(), mappings: [mapping], thinIds: [7] };
  expect(() => verifyReleasedMappings(pool, retained)).not.toThrow();
  for (const patch of [
    { open: 1 },
    { device: "253:2" },
    { id: 8 },
    { name: "suite.sb-" + "b".repeat(32) + "-root" },
  ])
    expect(() =>
      verifyReleasedMappings(
        { ...pool, mappings: [{ ...mapping, ...patch }] },
        retained,
      ),
    ).toThrow();
  expect(() => verifyThinIds({ ...pool, thinIds: [7, 8] })).toThrow();
});

test("managed boot selects only the current exact API socket and retains its disk contract", async () => {
  const { managedBootPaths, selectManagedProcess } =
    await import("./fc-runtime.ts");
  const sandbox = "11111111-1111-4111-8111-111111111111";
  const one = managedBootPaths("/owned/host", sandbox, "boot-1");
  const next = managedBootPaths("/owned/host", sandbox, "boot-2");
  expect(one.directory).toBe("/owned/host/m-548f58de1d3dd34c94a49d74");
  expect(one.directory).not.toBe(next.directory);
  expect(one.rootfsCanonical).toBe(one.directory + "/rootfs.ext4");
  expect(one.volume).toBe("sb-11111111111141118111111111111111-root");
  const args = ["/usr/local/bin/firecracker", "--api-sock", one.apiSocket];
  const process = {
    pid: 42,
    birth: "100",
    bootId: "kernel",
    uid: 0,
    executable: args[0]!,
    cgroup: "/owned",
    sha256: "a".repeat(64),
  };
  expect(selectManagedProcess([{ process, args }], one.apiSocket)).toEqual(
    process,
  );
  expect(
    selectManagedProcess([{ process, args }], next.apiSocket),
  ).toBeUndefined();
  expect(() =>
    selectManagedProcess(
      [{ process, args: [...args, "--extra"] }],
      one.apiSocket,
    ),
  ).toThrow("fc_managed_process_arguments");
  expect(() =>
    selectManagedProcess(
      [
        { process, args },
        { process: { ...process, pid: 43 }, args },
      ],
      one.apiSocket,
    ),
  ).toThrow("fc_managed_process_duplicate");
});

test("managed coverage uses durable start prerequisites and never forgets an observed attempt", async () => {
  const { requireManagedBootCoverage } = await import("./fc-runtime.ts");
  const required = new Set<string>();
  const proof = new Set<string>();
  requireManagedBootCoverage(required, proof, "prepared", {
    finalization: null,
    state: { state: "prepared" },
    capture: null,
  });
  expect(required.size).toBe(0);
  expect(() =>
    requireManagedBootCoverage(required, proof, "retired", {
      finalization: "sha256:abc",
      state: { state: "retired" },
      capture: null,
    }),
  ).toThrow("fc_physical_boot_unobserved");
  expect(required.has("retired")).toBe(true);
  expect(() =>
    requireManagedBootCoverage(required, proof, "retired", {
      finalization: null,
      state: { state: "failed" },
      capture: null,
    }),
  ).toThrow("fc_physical_boot_unobserved");
  expect(() =>
    requireManagedBootCoverage(required, proof, "live", {
      finalization: "sha256:abc",
      state: { state: "started" },
      capture: null,
    }),
  ).toThrow("fc_physical_boot_unobserved");
  proof.add("retired");
  proof.add("live");
  expect(() =>
    requireManagedBootCoverage(required, proof, "retired", {
      finalization: "sha256:abc",
      state: { state: "retired" },
      capture: null,
    }),
  ).not.toThrow();
});

test("unknown managed state cannot claim a never-started exemption", async () => {
  const { requireManagedBootCoverage } = await import("./fc-runtime.ts");
  expect(() =>
    requireManagedBootCoverage(new Set(), new Set(), "unknown", {
      finalization: null,
      state: { state: "unknown" },
      capture: null,
    }),
  ).toThrow("fc_managed_boot_state");
});

test("metadata inspection cannot release a replacement host reservation", async () => {
  const access = new OfflinePoolAccess();
  access.start();
  let owner: "none" | "adapter" | "host" = "none";
  const released: string[] = [];
  const read = () =>
    access.inspect(
      async () => {},
      (lease) =>
        reservedThinIds(
          "suite",
          "/dev/loop0",
          poolFixture().status,
          lease,
          async (args) => {
            if (args.at(-1) === "reserve_metadata_snap") owner = "adapter";
            else {
              released.push(owner);
              owner = "none";
            }
            return "";
          },
          async () => {
            // This host reader can replace a reservation only while its process runs.
            owner = "none";
            owner = "host";
            return '<superblock><device dev_id="7"/></superblock>';
          },
          { pending: Promise.resolve(), failed: false },
        ),
    );
  await expect(read()).rejects.toThrow("fc_pool_host_not_quiescent");
  expect(released).not.toContain("host");
  expect(owner).toBe("none");
});

test("offline reservation excludes a host restart until release and invalidates its lease", async () => {
  const access = new OfflinePoolAccess();
  access.start();
  let live = true;
  await expect(
    access.stopped(async () => {
      if (live) throw Error("host still live");
    }),
  ).rejects.toThrow("host still live");
  await expect(
    access.inspect(
      async () => {},
      async () => [],
    ),
  ).rejects.toThrow("fc_pool_host_not_quiescent");
  live = false;
  await access.stopped(async () => {
    expect(live).toBe(false);
  });
  let owner = "none";
  const released: string[] = [];
  let expired: OfflinePoolLease | undefined;
  const result = await access.inspect(
    async () => {
      expect(live).toBe(false);
    },
    async (lease) => {
      expired = lease;
      return reservedThinIds(
        "suite",
        "/dev/loop0",
        poolFixture().status,
        lease,
        async (args) => {
          if (args.at(-1) === "reserve_metadata_snap") owner = "adapter";
          else {
            released.push(owner);
            owner = "none";
          }
          return "";
        },
        async () => {
          // A replacement host would release and reserve here; the start barrier
          // refuses to launch it while the adapter owns the offline lease.
          expect(() => access.start()).toThrow(
            "fc_metadata_host_start_blocked",
          );
          return '<superblock><device dev_id="7"/></superblock>';
        },
        { pending: Promise.resolve(), failed: false },
      );
    },
  );
  expect(result).toEqual([7]);
  expect(released).toEqual(["adapter"]);
  expect(owner).toBe("none");
  expect(() => access.start()).not.toThrow();
  if (!expired) throw Error("lease missing");
  await expect(
    reservedThinIds(
      "suite",
      "/dev/loop0",
      poolFixture().status,
      expired,
      async () => {
        throw Error("must not access metadata");
      },
      async () => "",
    ),
  ).rejects.toThrow("fc_pool_host_not_quiescent");
});
