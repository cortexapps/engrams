// Controlled unit-test state; never physical acceptance evidence.
export const poolFixture = () => ({
  name: "suite",
  directory: "/owned/pool",
  device: "253:0",
  path: "/dev/dm-0",
  table: "0 268435456 thin-pool 7:0 7:1 128 32768 0",
  major: "253",
  node: { path: "/dev/dm-0", device: "253:0", ino: "5", rdev: "64768" },
  otherTables: "",
  metadata: {
    path: "/dev/loop0",
    device: "7:0",
    ino: "1",
    rdev: "1792",
    file: "/owned/pool/metadata.img",
    fileDev: "4",
    fileIno: "11",
    bytes: "1073741824",
    allocated: "1073741824",
  },
  data: {
    path: "/dev/loop1",
    device: "7:1",
    ino: "2",
    rdev: "1793",
    file: "/owned/pool/data.img",
    fileDev: "4",
    fileIno: "12",
    bytes: "137438953472",
    allocated: "137438953472",
  },
  control: {
    path: "/dev/mapper/control",
    ino: "3",
    rdev: "2700",
    device: "10:236",
  },
  status:
    "0 268435456 thin-pool 0 10/262144 0/2097152 - rw no_discard_passdown queue_if_no_space - 1024",
  thinIds: [] as number[],
  mappings: [] as {
    name: string;
    device: string;
    table: string;
    id: number;
    open: number;
  }[],
  holders: [] as string[],
  fds: [] as { pid: number; fd: string; rdev: string }[],
});
