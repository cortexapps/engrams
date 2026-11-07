import {
  FULL_MATRIX,
  selectedCases,
  type RunSelection,
} from "../../../../orchestrator/scripts/mcp-stack/manifest.ts";
import { readFile, access, lstat, open, readdir } from "node:fs/promises";
import { constants } from "node:fs";
import { arch, platform } from "node:os";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  check,
  childEnvironment,
  command,
  ownedPath,
  PlatformFailure,
  privateRoot,
  readOwned,
  record,
  verifyPin,
  type Pin,
} from "./owned.ts";

export const REPO = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../../..",
);
export const BINARIES = [
  "node",
  "bun",
  "docker",
  "openssl",
  "hostAgent",
  "kernel",
  "helper",
  "claudeHarness",
  "codexHarness",
  "modelCli",
  "agentdBundle",
  "sentinelBundle",
  "guestToolsBundle",
  "guestInit",
] as const;
export const IMAGES = [
  "postgres",
  "blob",
  "registry",
  "dns",
  "edge",
  "guard",
  "coordinator",
  "orchestrator",
  "worker",
  "fixture",
  "guest",
] as const;
export interface ImagePin {
  archive: Pin;
  id: string;
  manifestDigest: string;
}
export interface Inputs {
  version: 1;
  backend: "firecracker" | "vz";
  architecture: "arm64" | "amd64";
  sourceCommit: string;
  engineSocket: string;
  engineConfig: string;
  engineId: string;
  registryPort: number;
  subnet: string;
  nativeHostIp: string;
  binaries: Record<(typeof BINARIES)[number], Pin>;
  images: Record<(typeof IMAGES)[number], ImagePin> & { hostAgent?: ImagePin };
  firecracker?: Pin;
  proxy: Pin;
  nativeProbe?: Pin;
}
function text(value: unknown, pattern: RegExp, seam: string): string {
  check(typeof value === "string" && pattern.test(value), seam);
  return value;
}
function pin(value: unknown): Pin {
  const p = record(value, ["path", "sha256"]);
  const path = text(p.path, /^\//, "artifact_path");
  check(isAbsolute(path) && resolve(path) === path, "artifact_path");
  return { path, sha256: text(p.sha256, /^[a-f0-9]{64}$/, "artifact_hash") };
}
export function parseInputs(
  value: unknown,
  backend: Inputs["backend"],
): Inputs {
  const o = record(
    value,
    [
      "version",
      "backend",
      "architecture",
      "sourceCommit",
      "engineSocket",
      "engineConfig",
      "engineId",
      "registryPort",
      "subnet",
      "nativeHostIp",
      "binaries",
      "images",
      "proxy",
    ],
    ["firecracker", "nativeProbe"],
  );
  check(
    o.version === 1 &&
      o.backend === backend &&
      ["arm64", "amd64"].includes(String(o.architecture)),
    "platform_identity",
  );
  if (backend === "vz")
    check(
      o.architecture === "arm64" && !o.firecracker && o.nativeProbe,
      "vz_architecture",
    );
  else check(o.firecracker && !o.nativeProbe, "fc_artifacts");
  check(
    Number.isInteger(o.registryPort) &&
      Number(o.registryPort) >= 20000 &&
      Number(o.registryPort) <= 60000,
    "registry_port",
  );
  const rawBins = record(o.binaries, BINARIES),
    rawImages = record(
      o.images,
      backend === "firecracker" ? [...IMAGES, "hostAgent"] : IMAGES,
    );
  const binaries = Object.fromEntries(
    BINARIES.map((key) => [key, pin(rawBins[key])]),
  ) as Inputs["binaries"];
  const images = Object.fromEntries(
    (backend === "firecracker" ? [...IMAGES, "hostAgent"] : [...IMAGES]).map(
      (key) => {
        const image = record(rawImages[key], [
          "archive",
          "id",
          "manifestDigest",
        ]);
        return [
          key,
          {
            archive: pin(image.archive),
            id: text(image.id, /^sha256:[a-f0-9]{64}$/, "image_digest"),
            manifestDigest: text(
              image.manifestDigest,
              /^sha256:[a-f0-9]{64}$/,
              "image_manifest_digest",
            ),
          },
        ];
      },
    ),
  ) as Inputs["images"];
  const subnet = text(
    o.subnet,
    /^10\.(?:[1-9]|[1-9][0-9]|1[0-9]{2}|2[0-4][0-9]|25[0-4])\.(?:0|[1-9][0-9]?|1[0-9]{2}|2[0-4][0-9]|25[0-4])\.0\/24$/,
    "private_subnet",
  );
  const nativeHostIp = text(
    o.nativeHostIp,
    /^(?:10|172|192)\.(?:[0-9]{1,3}\.){2}[0-9]{1,3}$/,
    "native_host_ip",
  );
  const octets = nativeHostIp.split(".").map(Number);
  check(
    octets.join(".") === nativeHostIp &&
      octets.every((v) => v < 256) &&
      (octets[0] === 10 ||
        (octets[0] === 172 && octets[1]! >= 16 && octets[1]! <= 31) ||
        (octets[0] === 192 && octets[1] === 168)),
    "native_host_ip",
  );
  check(!nativeHostIp.startsWith(subnet.slice(0, -4)), "separate_host_route");
  return {
    version: 1,
    backend,
    architecture: o.architecture as Inputs["architecture"],
    sourceCommit: text(o.sourceCommit, /^[a-f0-9]{40}$/, "source_commit"),
    engineSocket: text(o.engineSocket, /^\//, "engine_socket"),
    engineConfig: text(o.engineConfig, /^\//, "engine_config"),
    engineId: text(o.engineId, /^[A-Za-z0-9:_-]{12,128}$/, "engine_identity"),
    registryPort: Number(o.registryPort),
    subnet,
    nativeHostIp,
    binaries,
    images,
    proxy: pin(o.proxy),
    ...(backend === "firecracker"
      ? { firecracker: pin(o.firecracker) }
      : { nativeProbe: pin(o.nativeProbe) }),
  };
}
export function engineArguments(
  inputs: Pick<Inputs, "engineSocket" | "engineConfig">,
  image?: string,
): string[] {
  for (const path of [inputs.engineSocket, inputs.engineConfig])
    check(isAbsolute(path) && resolve(path) === path, "engine_path");
  if (image !== undefined)
    check(/^sha256:[a-f0-9]{64}$/.test(image), "image_digest");
  return [
    "--config",
    inputs.engineConfig,
    "--host",
    `unix://${inputs.engineSocket}`,
    ...(image === undefined
      ? ["info", "--format", "{{json .}}"]
      : ["image", "inspect", image]),
  ];
}
export async function validateEngineConfig(
  path: string,
  expected?: { ino: number; dev: number; mtimeMs: number; ctimeMs: number },
) {
  await privateRoot(path);
  const state = await lstat(path);
  check((await readdir(path)).length === 0, "engine_config_not_empty");
  if (expected)
    check(
      state.ino === expected.ino &&
        state.dev === expected.dev &&
        state.mtimeMs === expected.mtimeMs &&
        state.ctimeMs === expected.ctimeMs,
      "engine_config_changed",
    );
  return {
    ino: state.ino,
    dev: state.dev,
    mtimeMs: state.mtimeMs,
    ctimeMs: state.ctimeMs,
  };
}
export function cli(args: string[]): {
  backend: Inputs["backend"];
  runRoot: string;
  manifest: string;
  selection: RunSelection;
} {
  check(
    [6, 8].includes(args.length) &&
      args[0] === "--backend" &&
      ["firecracker", "vz"].includes(args[1]!) &&
      args[2] === "--run-root" &&
      args[4] === "--artifacts",
    "arguments",
  );
  for (const p of [args[3]!, args[5]!])
    check(isAbsolute(p) && resolve(p) === p, "arguments_path");
  let selection: RunSelection = FULL_MATRIX;
  if (args.length === 8) {
    check(
      args[6] === "--matrix" ||
        (args[6] === "--stage" && args[7] === "infrastructure"),
      "arguments",
    );
    selection =
      args[6] === "--stage"
        ? { stage: "infrastructure" }
        : { stage: "matrix", matrix: args[7]! };
  }
  selectedCases(selection);
  return {
    backend: args[1] as Inputs["backend"],
    runRoot: args[3]!,
    manifest: args[5]!,
    selection,
  };
}
export function requirePlatform(
  backend: Inputs["backend"],
  architecture: Inputs["architecture"],
  os = platform(),
  cpu = arch(),
): void {
  if (
    os !== (backend === "vz" ? "darwin" : "linux") ||
    cpu !== (architecture === "amd64" ? "x64" : "arm64")
  )
    throw new PlatformFailure("NEEDS_CONTEXT", "target_platform");
}
export function binaryArchitecture(bytes: Uint8Array): {
  os: "linux" | "darwin";
  architecture: Inputs["architecture"];
} {
  const data = Buffer.from(bytes);
  check(data.length >= 64, "binary_header");
  if (data.subarray(0, 4).equals(Buffer.from([0x7f, 69, 76, 70]))) {
    check(data[4] === 2 && data[5] === 1, "elf_format");
    const machine = data.readUInt16LE(18);
    check(machine === 62 || machine === 183, "elf_architecture");
    return { os: "linux", architecture: machine === 62 ? "amd64" : "arm64" };
  }
  check(
    data.readUInt32LE(0) === 0xfeedfacf && data.readUInt32LE(4) === 0x0100000c,
    "macho_architecture",
  );
  return { os: "darwin", architecture: "arm64" };
}
async function header(path: string): Promise<Buffer> {
  const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const data = Buffer.alloc(1024);
    const { bytesRead } = await file.read(data, 0, data.length, 0);
    return data.subarray(0, bytesRead);
  } finally {
    await file.close();
  }
}
export async function validateExecutables(inputs: Inputs): Promise<void> {
  for (const name of ["node", "bun", "docker", "openssl"] as const)
    await access(inputs.binaries[name].path, constants.X_OK);
  const validate = async (pin: Pin, os: "linux" | "darwin") => {
    await access(pin.path, constants.X_OK);
    const actual = binaryArchitecture(await header(pin.path));
    check(
      actual.architecture === inputs.architecture && actual.os === os,
      "binary_platform",
    );
  };
  for (const role of [
    "hostAgent",
    "helper",
    "claudeHarness",
    "codexHarness",
    "modelCli",
  ] as const) {
    const os =
      ["hostAgent", "helper"].includes(role) && inputs.backend === "vz"
        ? "darwin"
        : "linux";
    await validate(inputs.binaries[role], os);
  }
  if (inputs.backend === "firecracker") {
    check(inputs.firecracker && inputs.proxy, "fc_artifacts");
    await validate(inputs.firecracker, "linux");
  }
  await validate(inputs.proxy, inputs.backend === "vz" ? "darwin" : "linux");
  if (inputs.backend === "vz") {
    check(inputs.nativeProbe, "native_probe");
    await validate(inputs.nativeProbe, "darwin");
  }
}
export async function validateVzEntitlement(
  hostAgent: string,
  env: NodeJS.ProcessEnv,
  runCommand: typeof command = command,
): Promise<void> {
  await runCommand(
    "/usr/bin/codesign",
    ["--verify", "--strict", hostAgent],
    REPO,
    env,
  );
  const entitlements = await runCommand(
    "/usr/bin/codesign",
    ["--display", "--entitlements", "-", "--xml", hostAgent],
    REPO,
    env,
  );
  const converted = await runCommand(
    "/usr/bin/plutil",
    ["-convert", "json", "-o", "-", "--", "-"],
    REPO,
    env,
    entitlements.toString("utf8"),
  );
  let value: unknown;
  try {
    value = JSON.parse(converted.toString("utf8"));
  } catch {
    throw new PlatformFailure("INVALID_CONTEXT", "vz_entitlement");
  }
  check(
    typeof value === "object" &&
      value !== null &&
      !Array.isArray(value) &&
      "com.apple.security.virtualization" in value &&
      value["com.apple.security.virtualization"] === true,
    "vz_entitlement",
  );
}
export async function preflight(
  backend: Inputs["backend"],
  runRoot: string,
  filename: string,
): Promise<Inputs> {
  const inputs = parseInputs(
    JSON.parse((await readOwned(filename)).toString("utf8")),
    backend,
  );
  requirePlatform(backend, inputs.architecture);
  await privateRoot(dirname(runRoot));
  await ownedPath(runRoot, true);
  // Never mutate an engine or create the output root when the complete matrix is absent.
  try {
    await access(
      join(REPO, "orchestrator/scripts/mcp-stack/run.ts"),
      constants.R_OK,
    );
  } catch {
    throw new PlatformFailure("NEEDS_CONTEXT", "common_runner");
  }
  for (const item of [
    ...Object.values(inputs.binaries),
    ...Object.values(inputs.images).map((v) => v.archive),
    ...(inputs.firecracker ? [inputs.firecracker] : []),
    inputs.proxy,
    ...(inputs.nativeProbe ? [inputs.nativeProbe] : []),
  ])
    await verifyPin(item);
  await validateExecutables(inputs);
  const kernel = await header(inputs.binaries.kernel.path);
  if (inputs.architecture === "arm64")
    check(
      kernel.length >= 64 && kernel.readUInt32LE(56) === 0x644d5241,
      "arm64_kernel",
    );
  else check(binaryArchitecture(kernel).architecture === "amd64", "fc_kernel");
  const env = childEnvironment(dirname(runRoot));
  const head = (
    await command("/usr/bin/git", ["-C", REPO, "rev-parse", "HEAD"], REPO, env)
  )
    .toString()
    .trim();
  check(head === inputs.sourceCommit, "source_commit");
  const dirty = await command(
    "/usr/bin/git",
    [
      "--no-optional-locks",
      "-C",
      REPO,
      "status",
      "--porcelain",
      "--untracked-files=no",
    ],
    REPO,
    env,
  );
  check(!dirty.length, "source_dirty");
  check(
    (await command(inputs.binaries.node.path, ["--version"], REPO, env))
      .toString()
      .startsWith("v24."),
    "node24",
  );
  check(
    (await command(inputs.binaries.openssl.path, ["version"], REPO, env))
      .toString()
      .startsWith("OpenSSL 3."),
    "openssl3",
  );
  await ownedPath(inputs.engineSocket);
  check((await lstat(inputs.engineSocket)).isSocket(), "engine_socket");
  const engineState = await lstat(inputs.engineSocket);
  const configState = await validateEngineConfig(inputs.engineConfig);
  const inspect = async (image?: string) => {
    await validateEngineConfig(inputs.engineConfig, configState);
    await ownedPath(inputs.engineSocket);
    const socket = await lstat(inputs.engineSocket);
    check(
      socket.isSocket() &&
        socket.ino === engineState.ino &&
        socket.dev === engineState.dev,
      "engine_socket_changed",
    );
    const bytes = await command(
      inputs.binaries.docker.path,
      engineArguments(inputs, image),
      REPO,
      childEnvironment(dirname(runRoot)),
    );
    await validateEngineConfig(inputs.engineConfig, configState);
    return bytes;
  };
  const info = JSON.parse((await inspect()).toString());
  check(
    info.ID === inputs.engineId &&
      info.OSType === "linux" &&
      [
        inputs.architecture,
        inputs.architecture === "amd64" ? "x86_64" : "aarch64",
      ].includes(info.Architecture),
    "engine_platform",
  );
  for (const [role, image] of Object.entries(inputs.images)) {
    const [actual] = JSON.parse((await inspect(image.id)).toString());
    check(
      actual?.Id === image.id &&
        actual?.Os === "linux" &&
        actual?.Architecture === inputs.architecture,
      "image_platform",
    );
    if (
      [
        "coordinator",
        "orchestrator",
        "worker",
        "fixture",
        "hostAgent",
      ].includes(role)
    )
      check(
        actual?.Config?.Labels?.["org.opencontainers.image.revision"] ===
          inputs.sourceCommit,
        "image_source",
      );
  }
  if (backend === "firecracker") {
    check(
      info.KernelVersion ===
        (await readFile("/proc/sys/kernel/osrelease", "utf8")).trim(),
      "fc_engine_kernel_release",
    );
    if (process.getuid?.() !== 0)
      throw new PlatformFailure("NEEDS_CONTEXT", "fc_privilege");
    try {
      await access("/dev/kvm", constants.R_OK | constants.W_OK);
      check((await lstat("/dev/kvm")).isCharacterDevice(), "kvm_device");
      await access("/dev/net/tun", constants.R_OK | constants.W_OK);
    } catch {
      throw new PlatformFailure("NEEDS_CONTEXT", "linux_kvm_tun");
    }
    const { inspectFcInventory } = await import("./fc-runtime.ts");
    await inspectFcInventory(inputs);
  } else {
    await validateVzEntitlement(inputs.binaries.hostAgent.path, env);
    check(
      (await command("/usr/sbin/sysctl", ["-n", "kern.hv_support"], REPO, env))
        .toString()
        .trim() === "1",
      "vz_hypervisor",
    );
  }
  return inputs;
}
