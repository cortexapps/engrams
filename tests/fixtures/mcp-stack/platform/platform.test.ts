import { poolFixture } from "./fc-thin-fixture.ts";
import { afterAll, beforeAll, expect, spyOn, test } from "bun:test";
import {
  chmod,
  link,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rename,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { request } from "node:http";
import { connect } from "node:net";
import { dirname, join } from "node:path";
import {
  ContextSchema,
  type Context,
} from "../../../../orchestrator/scripts/mcp-stack/context.ts";
import {
  ROLES,
  type SupervisorStatus,
} from "../../../../orchestrator/scripts/mcp-stack/supervisor.ts";
import {
  childEnvironment,
  command,
  hash,
  newRoot,
  OwnedProcess,
  PlatformFailure,
  privateRoot,
  put,
  readOwned,
  record,
  verifyPin,
} from "./owned.ts";
import {
  BINARIES,
  binaryArchitecture,
  engineArguments,
  validateEngineConfig,
  validateExecutables,
  validateVzEntitlement,
  cli,
  IMAGES,
  parseInputs,
  REPO,
  requirePlatform,
  type Inputs,
} from "./preflight.ts";
import { effectiveRules, rules } from "./network.ts";
import { parseOperation, startSupervisor, verifyStatus } from "./supervisor.ts";
import { generateIdentity } from "./identity.ts";
import { generateConfiguration } from "./configuration.ts";

import { Engine } from "./engine.ts";
import { verifyFcKernelPosture } from "./fc-runtime.ts";

test("FC kernel posture requires writable private proc sys and exactly four capabilities", () => {
  const mounts = [
    "31 20 0:27 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw",
    "32 20 0:28 / /sys ro,nosuid,nodev,noexec,relatime - sysfs sysfs ro",
    "33 32 0:29 / /sys/fs/cgroup ro,nosuid,nodev,noexec,relatime - cgroup2 cgroup rw",
  ].join("\n");
  const status =
    "CapEff:\t0000000008201400\nCapPrm:\t0000000008201400\nCapBnd:\t0000000008201400\nNoNewPrivs:\t1\nSeccomp:\t2\n";
  expect(verifyFcKernelPosture(mounts, status)).toHaveLength(3);
  expect(() =>
    verifyFcKernelPosture(
      mounts + "\n34 31 0:27 /sys /proc/sys rw - proc proc rw",
      status,
    ),
  ).not.toThrow();
  for (const changed of [
    mounts.replace("/proc rw,", "/proc ro,"),
    mounts + "\n34 31 0:27 /sys /proc/sys ro - proc proc rw",
    mounts.replace("/sys ro,", "/sys rw,"),
    mounts.replace("/sys/fs/cgroup ro,", "/sys/fs/cgroup rw,"),
    mounts
      .split("\n")
      .filter((line) => !line.includes(" /proc "))
      .join("\n"),
  ])
    expect(() => verifyFcKernelPosture(changed, status)).toThrow();
  for (const field of ["CapEff", "CapPrm", "CapBnd"]) {
    for (const mask of ["0000000008201000", "0000000008281400"]) {
      expect(() =>
        verifyFcKernelPosture(
          mounts,
          status.replace(field + ":\t0000000008201400", field + ":\t" + mask),
        ),
      ).toThrow("fc_host_effective_capabilities");
    }
  }
  for (const changed of [
    status.replace("NoNewPrivs:\t1", "NoNewPrivs:\t0"),
    status.replace("Seccomp:\t2", "Seccomp:\t0"),
  ])
    expect(() => verifyFcKernelPosture(mounts, changed)).toThrow(
      "fc_host_effective_confinement",
    );
});

test("the disposable host TLS probe stops without delaying other roles", async () => {
  const runId = "mcp-123456789012345678901234";
  const id = "a".repeat(64);
  const engine = new Engine(input(), "/owned", runId);
  let running = false;
  const stops: string[][] = [];
  spyOn(engine, "audit").mockImplementation(async () => ({}));
  spyOn(engine, "call").mockImplementation(async (args) => {
    if (args[0] === "create") return Buffer.from(id);
    if (args[0] === "start") running = true;
    if (args[0] === "stop") {
      stops.push(args);
      running = false;
    }
    if (args[0] === "container" && args[1] === "inspect")
      return Buffer.from(
        JSON.stringify([
          {
            Id: id,
            Config: { Labels: { "engrams.mcp.run": runId } },
            State: { Running: running, Pid: running ? 1 : 0 },
          },
        ]),
      );
    return Buffer.alloc(0);
  });
  await engine.create(
    "host-tls-probe",
    "guard",
    [],
    ["infinity"],
    "/bin/sleep",
  );
  await engine.remove("host-tls-probe");
  await engine.create(
    "ordinary-probe",
    "guard",
    [],
    ["infinity"],
    "/bin/sleep",
  );
  await engine.remove("ordinary-probe");
  expect(stops).toEqual([
    ["stop", "--time", "0", id],
    ["stop", "--time", "3", id],
  ]);
});

let root: string;
let contextCount = 0;
const node = process.execPath;
beforeAll(async () => {
  const parent = join(REPO, "var/mcp-platform-bootstrap");
  await mkdir(parent, { mode: 0o700, recursive: true });
  await privateRoot(parent);
  root = await mkdtemp(join(parent, "t-"));
});
afterAll(async () => {
  if (root) await rm(root, { recursive: true });
});
function input(): Inputs {
  const pin = { path: "/owned/artifact", sha256: "a".repeat(64) };
  return parseInputs(
    {
      version: 1,
      backend: "vz",
      proxy: pin,
      nativeProbe: pin,
      architecture: "arm64",
      sourceCommit: "b".repeat(40),
      engineSocket: "/owned/engine.sock",
      engineConfig: "/owned/engine-config",
      engineId: "fixture-engine-identity",
      registryPort: 24567,
      subnet: "10.80.1.0/24",
      nativeHostIp: "192.168.50.2",
      binaries: Object.fromEntries(BINARIES.map((v) => [v, pin])),
      images: Object.fromEntries(
        IMAGES.map((v) => [
          v,
          {
            archive: pin,
            id: `sha256:${"c".repeat(64)}`,
            manifestDigest: `sha256:${"d".repeat(64)}`,
          },
        ]),
      ),
    },
    "vz",
  );
}
async function context(): Promise<Context> {
  const ctxRoot = await mkdtemp(join(root, "c-"));
  const pin = await put(join(ctxRoot, "normal"), "normal-configuration");
  await put(
    join(ctxRoot, "token"),
    "fixture-supervisor-token-" + "a".repeat(32),
  );
  const endpoint = {
    origin: "https://app.fixture.example",
    ca: pin,
    serverFingerprint: Array(32).fill("AA").join(":"),
  };
  const fixture = (kind: string) => ({
    ...endpoint,
    origin: `https://observer.${kind}.fixture.example`,
    configuration: pin,
    runRoot: ctxRoot,
    tokenFile: join(ctxRoot, "token"),
    resourceKeyFile: join(ctxRoot, "token"),
    resourceOrigin: `https://resource.${kind}.fixture.example`,
    issuerOrigin: `https://issuer.${kind}.fixture.example`,
    authorizationOrigin: `https://authorization.${kind}.fixture.example`,
    tokenOrigin: `https://token.${kind}.fixture.example`,
    redirectUri: "https://app.fixture.example/integrations/mcp/callback",
  });
  const human = (kind: string) => ({
    email: `${kind}@fixture.example`,
    passwordFile: join(ctxRoot, `${kind}.password`),
    cookieFile: join(ctxRoot, `${kind}.cookie`),
  });
  return ContextSchema.parse({
    version: 1,
    runId: "fixture-run",
    runRoot: root,
    backend: "vz",
    sourceCommit: "a".repeat(40),
    hostId: "00000000-0000-4000-8000-000000000001",
    imageId: "fixture-image",
    imageDigest: `sha256:${"b".repeat(64)}`,
    artifacts: Object.fromEntries(
      [
        "coordinator",
        "orchestrator",
        "worker",
        "hostAgent",
        "fixture",
        "kernel",
        "guestImage",
        "claudeHarness",
        "codexHarness",
        "modelCli",
        "claudeBundle",
        "codexBundle",
        "helper",
      ].map((v) => [v, pin]),
    ),
    endpoints: {
      app: endpoint,
      coordinator: endpoint,
      operator: { ...endpoint, certificate: pin, privateKeyFile: pin.path },
      workerControl: endpoint,
    },
    protocolFixtures: {
      modern: { ...fixture("modern"), protocol: "2026-07-28" },
      legacy: { ...fixture("legacy"), protocol: "2025-06-18" },
    },
    publication: {
      candidateIdentity: `sha256:${"b".repeat(64)}`,
      expectedGeneration: null,
      deploymentIdentity: `sha256:${"b".repeat(64)}`,
      networkIdentity: `sha256:${"b".repeat(64)}`,
      trustIdentity: `sha256:${"b".repeat(64)}`,
      zone: "fixture-zone",
    },
    identities: {
      admin: human("admin"),
      owner: human("owner"),
      other: human("other"),
      unattendedKeyFile: join(ctxRoot, "unattended"),
      automationKeyFile: join(ctxRoot, "automation"),
      routerSentinelFile: join(ctxRoot, "router"),
    },
    supervisor: {
      socket: join(root, String(++contextCount)),
      tokenFile: join(ctxRoot, "token"),
      configurations: {
        orchestrator: {
          normal: pin,
          "global-disabled": pin,
          "zone-disabled": pin,
        },
        coordinator: { normal: pin },
        worker: { normal: pin },
        "host-agent": { normal: pin },
      },
      operatorCandidates: {
        normal: { file: pin, identity: `sha256:${"b".repeat(64)}` },
      },
    },
    observer: {
      orchestratorDsnFile: pin.path,
      coordinatorDsnFile: pin.path,
      hostManagedRoot: ctxRoot,
      hostLogFile: pin.path,
    },
    guest: {
      stateDirectory: "/tmp/mcp-stack-fixture",
      cliPath: "/opt/engram/dyn/0/engram-mcp-model-fixture",
    },
    deadlines: { requestMs: 1000, stepMs: 3000, totalMs: 10000 },
    evidenceFile: join(ctxRoot, "evidence"),
  });
}
function status(ctx: Context): SupervisorStatus {
  return {
    version: 1,
    runId: ctx.runId,
    backend: ctx.backend,
    sourceCommit: ctx.sourceCommit,
    hostId: ctx.hostId,
    roles: ROLES.map((role) => ({
      role,
      state: "running",
      bootId: role + "-first",
      artifactSha256: ctx.artifacts.fixture.sha256,
      configurationId: "normal",
      configurationSha256: ctx.artifacts.fixture.sha256,
    })),
    network: {
      generation: "fixture-generation",
      enforcementSha256: "e".repeat(64),
      fixtureOnlyEgress: true,
      externalDnsBlocked: true,
      roles: ["orchestrator", "coordinator", "worker"],
    },
  };
}

test("private input refuses symlink, hardlink, public mode and changed pin", async () => {
  const pin = await put(join(root, "input"), "bytes");
  await verifyPin(pin);
  await symlink(pin.path, join(root, "symlink"));
  await expect(readOwned(join(root, "symlink"))).rejects.toThrow();
  await link(pin.path, join(root, "hardlink"));
  await expect(verifyPin(pin)).rejects.toThrow("artifact_identity");
  await rm(join(root, "hardlink"));
  await chmod(pin.path, 0o644);
  await expect(readOwned(pin.path)).rejects.toThrow("private_file");
  await chmod(pin.path, 0o600);
  await writeFile(pin.path, "changed");
  await expect(verifyPin(pin)).rejects.toThrow("artifact_hash");
});
test("output root refuses reuse and symlinked parent", async () => {
  const fresh = join(root, "fresh");
  await newRoot(fresh);
  await expect(newRoot(fresh)).rejects.toThrow("output_exists");
  await symlink(fresh, join(root, "linked"));
  await expect(newRoot(join(root, "linked", "child"))).rejects.toThrow(
    "real_path",
  );
});
test("arguments and manifest reject foreign backend, arbitrary commands and mixed architecture", () => {
  expect(
    cli([
      "--backend",
      "vz",
      "--run-root",
      "/private/run",
      "--artifacts",
      "/private/input",
    ]).backend,
  ).toBe("vz");
  expect(() =>
    cli([
      "--backend",
      "vz",
      "--run-root",
      "relative",
      "--artifacts",
      "/private/input",
    ]),
  ).toThrow();
  expect(() =>
    parseInputs({ ...input(), command: ["sh", "-c", "anything"] }, "vz"),
  ).toThrow();
  expect(() =>
    parseInputs({ ...input(), architecture: "amd64" }, "vz"),
  ).toThrow("vz_architecture");
  expect(() => parseInputs(input(), "firecracker")).toThrow(
    "platform_identity",
  );
  expect(() =>
    requirePlatform("firecracker", "arm64", "darwin", "arm64"),
  ).toThrow("NEEDS_CONTEXT:target_platform");
});
test("artifact architecture comes from executable bytes", () => {
  const elf = Buffer.alloc(64);
  Buffer.from([0x7f, 69, 76, 70, 2, 1]).copy(elf);
  elf.writeUInt16LE(183, 18);
  expect(binaryArchitecture(elf)).toEqual({
    os: "linux",
    architecture: "arm64",
  });
  elf.writeUInt16LE(62, 18);
  expect(binaryArchitecture(elf)).toEqual({
    os: "linux",
    architecture: "amd64",
  });
  elf[5] = 2;
  expect(() => binaryArchitecture(elf)).toThrow("elf_format");
  const macho = Buffer.alloc(64);
  macho.writeUInt32LE(0xfeedfacf);
  macho.writeUInt32LE(0x0100000c, 4);
  expect(binaryArchitecture(macho)).toEqual({
    os: "darwin",
    architecture: "arm64",
  });
  expect(() => binaryArchitecture(Buffer.from("a success receipt"))).toThrow(
    "binary_header",
  );
});
test("VZ entitlement verifies first and converts explicit XML through bounded commands", async () => {
  const hostAgent = "/owned/host-agent";
  const env = childEnvironment(root);
  const xml =
    '<?xml version="1.0"?><plist version="1.0"><dict><key>com.apple.security.virtualization</key><true/></dict></plist>';
  const calls: Parameters<typeof command>[] = [];
  const runCommand: typeof command = async (...args) => {
    calls.push(args);
    return Buffer.from(
      ["", xml, '{"com.apple.security.virtualization":true}'][
        calls.length - 1
      ]!,
    );
  };
  await validateVzEntitlement(hostAgent, env, runCommand);
  expect(calls).toEqual([
    ["/usr/bin/codesign", ["--verify", "--strict", hostAgent], REPO, env],
    [
      "/usr/bin/codesign",
      ["--display", "--entitlements", "-", "--xml", hostAgent],
      REPO,
      env,
    ],
    [
      "/usr/bin/plutil",
      ["-convert", "json", "-o", "-", "--", "-"],
      REPO,
      env,
      xml,
    ],
  ]);
  for (const call of calls) expect(call[3]).toBe(env);
});
for (const [kind, json] of [
  ["false", '{"com.apple.security.virtualization":false}'],
  ["missing", "{}"],
  ["string", '{"com.apple.security.virtualization":"true"}'],
  ["number", '{"com.apple.security.virtualization":1}'],
  ["nested", '{"nested":{"com.apple.security.virtualization":true}}'],
  ["array", '[{"com.apple.security.virtualization":true}]'],
  ["null", "null"],
  ["scalar", "true"],
  ["malformed", '{"com.apple.security.virtualization":true'],
  ["text", "[Dict]\n[Key] com.apple.security.virtualization\n[Bool] true"],
] as const) {
  test(`VZ entitlement rejects ${kind} conversion output`, async () => {
    let calls = 0;
    const runCommand: typeof command = async () => {
      calls++;
      // A true XML-looking fragment must not bypass the OS conversion result.
      return Buffer.from(
        calls === 3
          ? json
          : "<key>com.apple.security.virtualization</key><true/>",
      );
    };
    await expect(
      validateVzEntitlement(
        "/owned/host-agent",
        childEnvironment(root),
        runCommand,
      ),
    ).rejects.toThrow("INVALID_CONTEXT:vz_entitlement");
    expect(calls).toBe(3);
  });
}
for (const step of [1, 2, 3]) {
  test(`VZ entitlement stops after command ${step} fails`, async () => {
    const failure = new PlatformFailure("FAILED", "command");
    let calls = 0;
    const runCommand: typeof command = async () => {
      if (++calls === step) throw failure;
      return Buffer.from("<key>com.apple.security.virtualization</key><true/>");
    };
    await expect(
      validateVzEntitlement(
        "/owned/host-agent",
        childEnvironment(root),
        runCommand,
      ),
    ).rejects.toBe(failure);
    expect(calls).toBe(step);
  });
}
test("controlled child sees no parent secret or runtime injection", async () => {
  process.env.MCP_PLATFORM_TEST_SECRET = "must-not-cross";
  try {
    const env = childEnvironment(root, {
      DOCKER_HOST: `unix://${root}/engine.sock`,
    });
    const result = JSON.parse(
      (
        await command(
          node,
          ["-e", "process.stdout.write(JSON.stringify(process.env))"],
          root,
          env,
        )
      ).toString(),
    );
    expect(result.DOCKER_HOST).toBe(`unix://${root}/engine.sock`);
    expect(result.MCP_PLATFORM_TEST_SECRET).toBeUndefined();
    expect(result.HOME).toBeUndefined();
    expect(result.NODE_OPTIONS).toBeUndefined();
    expect(() =>
      childEnvironment(root, { NODE_OPTIONS: "--require=bad" }),
    ).toThrow();
    expect(() =>
      childEnvironment(root, { AWS_ACCESS_KEY_ID: "parent-secret" }),
    ).toThrow();
    expect(() =>
      childEnvironment(root, { DOCKER_HOST: "tcp://remote:2375" }),
    ).toThrow();
  } finally {
    delete process.env.MCP_PLATFORM_TEST_SECRET;
  }
});
test("owned process stop escalates a TERM-resistant controlled child and waits", async () => {
  const marker = join(root, "child-ready");
  const child = new OwnedProcess(
    node,
    [
      "-e",
      "process.on('SIGTERM',()=>{});require('node:fs').writeFileSync(process.argv[1],'ready',{mode:384});setInterval(()=>{},1000)",
      marker,
    ],
    root,
    childEnvironment(root),
  );
  try {
    const deadline = performance.now() + 2000;
    while (performance.now() < deadline) {
      try {
        await readFile(marker);
        break;
      } catch {
        await new Promise((r) => setTimeout(r, 10));
      }
    }
    expect(child.running()).toBe(true);
    await child.stop(50);
    expect(child.running()).toBe(false);
    expect(child.child.signalCode).toBe("SIGKILL");
  } finally {
    await child.stop(50);
  }
});
test("network verifier rejects missing default drop, foreign accept and embedded-DNS allowance", () => {
  const plan = rules(input(), "worker");
  const actual = {
    nftables: [
      { metainfo: { json_schema_version: 1 } },
      ...plan.map((v) => {
        const [kind, body] = Object.entries(v)[0]!;
        return { [kind]: { ...(body as Record<string, unknown>), handle: 12 } };
      }),
    ],
  };
  expect(effectiveRules(actual, plan)).toMatch(/^[a-f0-9]{64}$/);
  const changed = structuredClone(actual);
  const chain = record(changed.nftables[2], ["chain"]).chain;
  record(chain, [
    "family",
    "table",
    "name",
    "type",
    "hook",
    "prio",
    "policy",
    "handle",
  ]).policy = "accept";
  expect(() => effectiveRules(changed, plan)).toThrow("network_rules_changed");
  expect(() =>
    effectiveRules(
      {
        nftables: [
          ...plan,
          {
            rule: {
              family: "inet",
              table: "mcp",
              chain: "output",
              expr: [{ accept: null }],
            },
          },
        ],
      },
      plan,
    ),
  ).toThrow();
  expect(JSON.stringify(plan)).toContain("127.0.0.11");
  expect(JSON.stringify(plan)).not.toContain('"dport"},"right":5432');
});
test("real private TLS leaves and normal/zone configs pass production validators", async () => {
  const directory = join(root, "identity");
  await mkdir(directory, { mode: 0o700 });
  const inputs = input();
  const opensslPath = await realpath(
    process.env.MCP_PLATFORM_TEST_OPENSSL ?? Bun.which("openssl") ?? "openssl",
  );
  inputs.binaries.openssl = {
    path: opensslPath,
    sha256: hash(await readFile(opensslPath)),
  };
  const identities = await generateIdentity(inputs, directory);
  expect(
    new Set(Object.values(identities.leaves).map((v) => v.fingerprint)).size,
  ).toBe(8);
  for (const leaf of Object.values(identities.leaves))
    expect(leaf.hostPin).toBe(
      leaf.fingerprint.replaceAll(":", "").toLowerCase(),
    );
  const configs = await generateConfiguration(inputs, directory, identities);
  const normal = JSON.parse(
    (await readOwned(configs.runtimeNormal.path)).toString(),
  );
  const disabled = JSON.parse(
    (await readOwned(configs.runtimeZoneDisabled.path)).toString(),
  );
  disabled.workers[0].enabled = true;
  expect(disabled).toEqual(normal);
  expect(configs.candidateIdentity).toBe(
    `sha256:${hash(await readOwned(configs.candidateFile.path))}`,
  );
  const c = JSON.parse((await readOwned(configs.coordinator.path)).toString());
  const modern = normal.registrations.find(
    (entry: { id: string }) => entry.id === "fixture-modern",
  );
  expect(modern.scopes).toEqual(["tools:read"]);
  expect(c.registry.registrations["fixture-modern"].scopes).toEqual([
    "tools:read",
  ]);
  expect(c.registry.registrations["fixture-modern"].accountField).toBe(
    "fixture_account",
  );
  expect(c.registry.registrations["fixture-legacy"].scopes).toEqual([
    "tools:read",
    "tools:write",
  ]);
  expect(c.registry.registrations["fixture-legacy"].accountField).toBe(
    "fixture_account",
  );
  expect(c.registry.zones[configs.zone.id].policyDigest).toHaveLength(64);
  expect(c.registry.zones[configs.zone.id].endpoints).toContain(
    "https://token.modern.fixture.example:8443/token",
  );
});
test("Unix supervisor authenticates, rejects payload authority and isolates restarts", async () => {
  const ctx = await context();
  let current = status(ctx),
    mutations = 0,
    corruptScope = false;
  const server = await startSupervisor(ctx, {
    status: async () => structuredClone(current),
    restart: async (role, configurationId) => {
      mutations++;
      const entry = current.roles.find((v) => v.role === role)!;
      entry.bootId += "-next";
      entry.configurationId = configurationId;
      if (corruptScope)
        current.roles.find((v) => v.role === "fixture-modern")!.bootId +=
          "-changed";
    },
    stop: async (role) => {
      mutations++;
      current.roles.find((v) => v.role === role)!.state = "stopped";
    },
  });
  const validToken = (await readOwned(ctx.supervisor.tokenFile)).toString();
  async function call(body: unknown, token = validToken) {
    const payload = JSON.stringify(body);
    return await new Promise<{ code: number; body: unknown }>(
      (resolve, reject) => {
        const req = request(
          {
            socketPath: ctx.supervisor.socket,
            method: "POST",
            path: "/v1/control",
            headers: {
              authorization: `Bearer ${token}`,
              "content-type": "application/json",
              "content-length": Buffer.byteLength(payload),
            },
          },
          (res) => {
            const chunks: Buffer[] = [];
            res.on("data", (v) => chunks.push(v));
            res.on("end", () =>
              resolve({
                code: res.statusCode!,
                body: JSON.parse(Buffer.concat(chunks).toString()),
              }),
            );
          },
        );
        req.on("error", reject);
        req.end(payload);
      },
    );
  }
  const base = { version: 1, runId: ctx.runId };
  try {
    expect((await call({ ...base, operation: "status" }, "wrong")).code).toBe(
      401,
    );
    expect(
      (await call({ ...base, runId: "other", operation: "status" })).code,
    ).toBe(400);
    expect(
      (
        await call({
          ...base,
          operation: "restart",
          role: "fixture-modern",
          configurationId: "normal",
        })
      ).code,
    ).toBe(400);
    expect(
      (
        await call({
          ...base,
          operation: "restart",
          role: "worker",
          configurationId: "zone-disabled",
        })
      ).code,
    ).toBe(400);
    expect(
      (await call({ ...base, operation: "status", command: "anything" })).code,
    ).toBe(400);
    expect(mutations).toBe(0);
    const before = structuredClone(current);
    expect(
      (
        await call({
          ...base,
          operation: "restart",
          role: "orchestrator",
          configurationId: "global-disabled",
        })
      ).code,
    ).toBe(200);
    expect(current.roles.find((v) => v.role === "fixture-modern")).toEqual(
      before.roles.find((v) => v.role === "fixture-modern"),
    );
    expect(() =>
      verifyStatus(ctx, { ...current, backend: "firecracker" }),
    ).toThrow("supervisor_identity");
    expect(() =>
      verifyStatus(ctx, { ...current, sourceCommit: "0".repeat(40) }),
    ).toThrow("supervisor_identity");
    expect(() =>
      parseOperation(
        {
          ...base,
          operation: "stop",
          role: "coordinator",
          configurationId: "normal",
        },
        ctx,
      ),
    ).toThrow();
    current = status(ctx);
    await writeFile(
      ctx.supervisor.configurations.orchestrator.normal.path,
      "changed",
    );
    const count = mutations;
    expect(
      (
        await call({
          ...base,
          operation: "restart",
          role: "orchestrator",
          configurationId: "normal",
        })
      ).code,
    ).toBe(400);
    expect(mutations).toBe(count);
    await writeFile(
      ctx.supervisor.configurations.orchestrator.normal.path,
      "normal-configuration",
    );
    corruptScope = true;
    expect(
      (
        await call({
          ...base,
          operation: "restart",
          role: "orchestrator",
          configurationId: "normal",
        })
      ).code,
    ).toBe(400);
    expect((await call({ ...base, operation: "status" })).code).toBe(503);
  } finally {
    await server.close();
  }
});

async function control(
  ctx: Context,
  body: unknown,
  headers: Record<string, string> = {},
) {
  const payload = JSON.stringify(body);
  const token = (await readOwned(ctx.supervisor.tokenFile)).toString();
  return await new Promise<{ code: number; text: string }>(
    (resolve, reject) => {
      const req = request(
        {
          socketPath: ctx.supervisor.socket,
          method: "POST",
          path: "/v1/control",
          headers: {
            authorization: `Bearer ${token}`,
            "content-type": "application/json",
            "content-length": String(Buffer.byteLength(payload)),
            ...headers,
          },
        },
        (res) => {
          const chunks: Buffer[] = [];
          res.on("data", (v) => chunks.push(v));
          res.on("error", reject);
          res.on("aborted", () => reject(new Error("response aborted")));
          res.on("end", () =>
            resolve({
              code: res.statusCode!,
              text: Buffer.concat(chunks).toString(),
            }),
          );
        },
      );
      req.on("error", reject);
      req.end(payload);
    },
  );
}

test("supervisor retains a safe diagnostic for a failed restart", async () => {
  const ctx = await context();
  ctx.runRoot = dirname(ctx.supervisor.tokenFile);
  ctx.supervisor.socket = join(ctx.runRoot, "s");
  const server = await startSupervisor(ctx, {
    status: async () => status(ctx),
    restart: async () => {
      throw new PlatformFailure("FAILED", "role_readiness");
    },
    stop: async () => {},
  });
  try {
    const response = await control(ctx, {
      version: 1,
      runId: ctx.runId,
      operation: "restart",
      role: "worker",
      configurationId: "normal",
    });
    expect(response.code).toBe(400);
    const diagnostic = JSON.parse(
      await readFile(join(ctx.runRoot, "supervisor-failure-1.json"), "utf8"),
    );
    expect(diagnostic).toEqual({
      version: 1,
      operation: "restart",
      role: "worker",
      phase: "lifecycle",
      failure: "FAILED",
      seam: "role_readiness",
    });
    expect(
      (
        await control(ctx, {
          version: 1,
          runId: ctx.runId,
          operation: "status",
        })
      ).code,
    ).toBe(503);
  } finally {
    await server.close();
  }
});

test("supervisor records a disconnected client without its request contents", async () => {
  const ctx = await context();
  ctx.runRoot = dirname(ctx.supervisor.tokenFile);
  ctx.supervisor.socket = join(ctx.runRoot, "s");
  let entered!: () => void, aborted!: () => void;
  const started = new Promise<void>((resolve) => {
    entered = resolve;
  });
  const stopped = new Promise<void>((resolve) => {
    aborted = resolve;
  });
  const server = await startSupervisor(ctx, {
    status: async (signal) => {
      entered();
      await new Promise<void>((resolve) =>
        signal.addEventListener(
          "abort",
          () => {
            aborted();
            resolve();
          },
          { once: true },
        ),
      );
      return status(ctx);
    },
    restart: async () => {},
    stop: async () => {},
  });
  const token = (await readOwned(ctx.supervisor.tokenFile)).toString();
  const payload = JSON.stringify({
    version: 1,
    runId: ctx.runId,
    operation: "status",
  });
  const req = request({
    socketPath: ctx.supervisor.socket,
    method: "POST",
    path: "/v1/control",
    headers: {
      authorization: `Bearer ${token}`,
      "content-type": "application/json",
      "content-length": Buffer.byteLength(payload),
    },
  });
  req.on("error", () => {});
  req.end(payload);
  await started;
  req.destroy();
  await stopped;
  await server.close();
  const diagnostic = JSON.parse(
    await readFile(join(ctx.runRoot, "supervisor-failure-1.json"), "utf8"),
  );
  expect(diagnostic).toEqual({
    version: 1,
    operation: "status",
    phase: "before_status",
    failure: "disconnect",
    seam: "control_aborted",
  });
  expect(JSON.stringify(diagnostic)).not.toContain(token);
});

test("supervisor records transition validation failure without changing control behavior", async () => {
  const ctx = await context();
  ctx.runRoot = dirname(ctx.supervisor.tokenFile);
  ctx.supervisor.socket = join(ctx.runRoot, "s");
  const current = status(ctx);
  const server = await startSupervisor(ctx, {
    status: async () => structuredClone(current),
    restart: async (role) => {
      current.roles.find((entry) => entry.role === role)!.bootId += "-next";
      current.roles.find((entry) => entry.role === "fixture-modern")!.bootId +=
        "-wrong";
    },
    stop: async () => {},
  });
  try {
    const response = await control(ctx, {
      version: 1,
      runId: ctx.runId,
      operation: "restart",
      role: "worker",
      configurationId: "normal",
    });
    expect(response.code).toBe(400);
    expect(
      JSON.parse(
        await readFile(join(ctx.runRoot, "supervisor-failure-1.json"), "utf8"),
      ),
    ).toEqual({
      version: 1,
      operation: "restart",
      role: "worker",
      phase: "transition",
      failure: "INVALID_CONTEXT",
      seam: "restart_scope",
    });
  } finally {
    await server.close();
  }
});

test("supervisor refuses duplicate authentication and oversized bodies before lifecycle access", async () => {
  const ctx = await context();
  let reads = 0;
  const server = await startSupervisor(ctx, {
    status: async () => {
      reads++;
      return status(ctx);
    },
    restart: async () => {},
    stop: async () => {},
  });
  try {
    expect(
      (await control(ctx, { operation: "status", extra: "x".repeat(4100) }))
        .code,
    ).toBe(400);
    const token = (await readOwned(ctx.supervisor.tokenFile)).toString();
    const response = await new Promise<string>((resolve, reject) => {
      const socket = connect(ctx.supervisor.socket);
      let result = "";
      socket.on("data", (bytes) => {
        result += bytes.toString();
      });
      socket.on("end", () => resolve(result));
      socket.on("error", reject);
      socket.once("connect", () =>
        socket.write(
          `POST /v1/control HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer ${token}\r\nAuthorization: Bearer ${token}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}`,
        ),
      );
    });
    expect(response).toContain("401");
    expect(reads).toBe(0);
  } finally {
    await server.close();
  }
});

test("supervisor has no control queue and shutdown aborts and drains the active lifecycle", async () => {
  const ctx = await context();
  let entered!: () => void;
  const started = new Promise<void>((resolve) => {
    entered = resolve;
  });
  let drained = false,
    mutations = 0;
  const server = await startSupervisor(ctx, {
    status: async (signal) => {
      entered();
      await new Promise<void>((resolve) =>
        signal.addEventListener(
          "abort",
          () => {
            drained = true;
            resolve();
          },
          { once: true },
        ),
      );
      return status(ctx);
    },
    restart: async () => {
      mutations++;
    },
    stop: async () => {
      mutations++;
    },
  });
  const body = {
    version: 1,
    runId: ctx.runId,
    operation: "restart",
    role: "worker",
    configurationId: "normal",
  };
  const pending = control(ctx, body).catch(() => undefined);
  await started;
  expect((await control(ctx, body)).code).toBe(409);
  await server.close();
  await pending;
  expect(drained).toBe(true);
  expect(mutations).toBe(0);
});

test("supervisor reports bounded cleanup failure if a lifecycle ignores cancellation", async () => {
  const ctx = await context();
  ctx.deadlines.requestMs = 50;
  let entered!: () => void, release!: () => void;
  const started = new Promise<void>((resolve) => {
    entered = resolve;
  });
  const stuck = new Promise<void>((resolve) => {
    release = resolve;
  });
  let mutations = 0;
  const server = await startSupervisor(ctx, {
    status: async () => {
      entered();
      await stuck;
      return status(ctx);
    },
    restart: async () => {
      mutations++;
    },
    stop: async () => {
      mutations++;
    },
  });
  const pending = control(ctx, {
    version: 1,
    runId: ctx.runId,
    operation: "restart",
    role: "worker",
    configurationId: "normal",
  }).catch(() => undefined);
  await started;
  try {
    await expect(server.close()).rejects.toThrow("FAILED:control_cleanup");
  } finally {
    release();
    await pending;
  }
  await new Promise((resolve) => setImmediate(resolve));
  expect(mutations).toBe(0);
});

test("supervisor does not expose lifecycle errors or permit later mutation", async () => {
  const ctx = await context();
  ctx.runRoot = dirname(ctx.supervisor.tokenFile);
  ctx.supervisor.socket = join(ctx.runRoot, "s");
  let mutations = 0;
  const server = await startSupervisor(ctx, {
    status: async () => {
      throw new Error("private credential must remain private");
    },
    restart: async () => {
      mutations++;
    },
    stop: async () => {
      mutations++;
    },
  });
  try {
    const body = {
      version: 1,
      runId: ctx.runId,
      operation: "stop",
      role: "worker",
    };
    const result = await control(ctx, body);
    expect(result).toEqual({ code: 400, text: '{"error":"control_failed"}' });
    const diagnostic = JSON.parse(
      await readFile(join(ctx.runRoot, "supervisor-failure-1.json"), "utf8"),
    );
    expect(diagnostic).toEqual({
      version: 1,
      operation: "stop",
      role: "worker",
      phase: "before_status",
      failure: "OTHER",
      seam: "other",
    });
    expect(JSON.stringify(diagnostic)).not.toContain("private credential");
    expect((await control(ctx, body)).code).toBe(503);
    expect(mutations).toBe(0);
  } finally {
    await server.close();
  }
});

test("supervisor cleanup refuses a substituted socket path and preserves the foreign file", async () => {
  const ctx = await context();
  const server = await startSupervisor(ctx, {
    status: async () => status(ctx),
    restart: async () => {},
    stop: async () => {},
  });
  const original = ctx.supervisor.socket + "-original";
  await rename(ctx.supervisor.socket, original);
  await put(ctx.supervisor.socket, "foreign file");
  try {
    await expect(server.close()).rejects.toThrow("socket_replaced");
    expect((await readOwned(ctx.supervisor.socket)).toString()).toBe(
      "foreign file",
    );
  } finally {
    // Restore the test-owned socket so the retained listener can close safely.
    await rm(ctx.supervisor.socket);
    await rename(original, ctx.supervisor.socket);
    await server.close();
  }
});

test("supervisor request deadline cancels status and refuses all later controls", async () => {
  const ctx = await context();
  ctx.runRoot = dirname(ctx.supervisor.tokenFile);
  ctx.supervisor.socket = join(ctx.runRoot, "s");
  ctx.deadlines.requestMs = 50;
  let aborted = false;
  const server = await startSupervisor(ctx, {
    status: async (signal) => {
      await new Promise<void>((resolve) =>
        signal.addEventListener(
          "abort",
          () => {
            aborted = true;
            resolve();
          },
          { once: true },
        ),
      );
      return status(ctx);
    },
    restart: async () => {},
    stop: async () => {},
  });
  const body = { version: 1, runId: ctx.runId, operation: "status" };
  try {
    await expect(control(ctx, body)).rejects.toThrow();
    expect(aborted).toBe(true);
    expect((await control(ctx, body)).code).toBe(503);
  } finally {
    await server.close();
  }
  expect(
    JSON.parse(
      await readFile(join(ctx.runRoot, "supervisor-failure-1.json"), "utf8"),
    ),
  ).toEqual({
    version: 1,
    operation: "status",
    phase: "before_status",
    failure: "deadline",
    seam: "control_aborted",
  });
});

test("bounded command rejects failed, oversized and timed-out children without error output", async () => {
  const env = childEnvironment(root);
  await expect(
    command(
      node,
      ["-e", "process.stderr.write('private-value');process.exit(3)"],
      root,
      env,
    ),
  ).rejects.toThrow("FAILED:command");
  await expect(
    command(
      node,
      [
        "-e",
        "process.stdout.write(Buffer.alloc(5*1024*1024));setInterval(()=>{},1000)",
      ],
      root,
      env,
    ),
  ).rejects.toThrow("FAILED:command");
  await expect(
    command(node, ["-e", "setInterval(()=>{},1000)"], root, env, undefined, 40),
  ).rejects.toThrow("FAILED:command");
  await expect(command("relative", [], root, env)).rejects.toThrow(
    "process_path",
  );
});

test("engine inspection uses only explicit Unix endpoint and private empty CLI configuration", async () => {
  const config = join(root, "docker-client");
  await newRoot(config);
  const initial = await validateEngineConfig(config);
  const inputs = {
    engineConfig: config,
    engineSocket: join(root, "engine.sock"),
  };
  expect(engineArguments(inputs)).toEqual([
    "--config",
    config,
    "--host",
    `unix://${inputs.engineSocket}`,
    "info",
    "--format",
    "{{json .}}",
  ]);
  const image = `sha256:${"b".repeat(64)}`;
  expect(engineArguments(inputs, image)).toEqual([
    "--config",
    config,
    "--host",
    `unix://${inputs.engineSocket}`,
    "image",
    "inspect",
    image,
  ]);
  expect(() => engineArguments(inputs, "image; arbitrary-command")).toThrow(
    "image_digest",
  );
  expect(() =>
    engineArguments({ ...inputs, engineSocket: "tcp://external" }),
  ).toThrow("engine_path");
  expect(Object.keys(childEnvironment(root)).sort()).toEqual([
    "LANG",
    "PATH",
    "TMPDIR",
  ]);
  await put(join(config, "config.json"), '{"auths":{"registry.example":{}}}');
  await expect(validateEngineConfig(config, initial)).rejects.toThrow(
    "engine_config_not_empty",
  );
  await rm(join(config, "config.json"));
  await rename(config, config + "-original");
  await newRoot(config);
  await expect(validateEngineConfig(config, initial)).rejects.toThrow(
    "engine_config_changed",
  );
  await rm(config, { recursive: true });
  await symlink(config + "-original", config);
  await expect(validateEngineConfig(config, initial)).rejects.toThrow(
    "real_path",
  );
});

test("supervisor rejects array role and configuration values before lifecycle access", async () => {
  const observed: { code: number; reads: number; mutations: number }[] = [];
  for (const fields of [
    { operation: "restart", role: ["worker"], configurationId: "normal" },
    { operation: "restart", role: "worker", configurationId: ["normal"] },
    { operation: "stop", role: ["worker"] },
  ]) {
    const ctx = await context();
    let reads = 0,
      mutations = 0;
    const server = await startSupervisor(ctx, {
      status: async () => {
        reads++;
        return status(ctx);
      },
      restart: async () => {
        mutations++;
      },
      stop: async () => {
        mutations++;
      },
    });
    try {
      const result = await control(ctx, {
        version: 1,
        runId: ctx.runId,
        ...fields,
      });
      observed.push({ code: result.code, reads, mutations });
    } finally {
      await server.close();
    }
  }
  expect(observed).toEqual(
    Array.from({ length: 3 }, () => ({ code: 400, reads: 0, mutations: 0 })),
  );
});

test("supervisor rejects declared non-O disable requests before lifecycle access", async () => {
  const observed: { code: number; reads: number; mutations: number }[] = [];
  for (const role of ["coordinator", "worker", "host-agent"] as const) {
    for (const configurationId of [
      "global-disabled",
      "zone-disabled",
    ] as const) {
      const ctx = await context();
      Reflect.set(
        ctx.supervisor.configurations[role],
        configurationId,
        ctx.supervisor.configurations[role].normal,
      );
      let reads = 0,
        mutations = 0;
      const current = status(ctx);
      const server = await startSupervisor(ctx, {
        status: async () => {
          reads++;
          return structuredClone(current);
        },
        restart: async (selected, variant) => {
          mutations++;
          const entry = current.roles.find((v) => v.role === selected)!;
          entry.bootId += "-next";
          entry.configurationId = variant;
        },
        stop: async () => {
          mutations++;
        },
      });
      try {
        const result = await control(ctx, {
          version: 1,
          runId: ctx.runId,
          operation: "restart",
          role,
          configurationId,
        });
        observed.push({ code: result.code, reads, mutations });
      } finally {
        await server.close();
      }
    }
  }
  expect(observed).toEqual(
    Array.from({ length: 6 }, () => ({ code: 400, reads: 0, mutations: 0 })),
  );
});

test("supervisor rejects declared non-O disable status", async () => {
  const ctx = await context();
  for (const role of ["coordinator", "worker", "host-agent"] as const) {
    for (const configurationId of [
      "global-disabled",
      "zone-disabled",
    ] as const) {
      Reflect.set(
        ctx.supervisor.configurations[role],
        configurationId,
        ctx.supervisor.configurations[role].normal,
      );
      const current = status(ctx);
      current.roles.find((v) => v.role === role)!.configurationId =
        configurationId;
      expect(() => verifyStatus(ctx, current)).toThrow(
        "supervisor_role_configuration",
      );
    }
  }
});

test("command waits for retained same-group descendants after leader exit", async () => {
  const marker = join(root, "command-descendant");
  const descendant =
    "process.on('SIGTERM',()=>{});require('node:fs').writeFileSync(process.argv[1],JSON.stringify({group:Number(process.argv[2]),child:process.pid}),{mode:384});setInterval(()=>{},1000)";
  const leader =
    "const fs=require('node:fs');const child=require('node:child_process').spawn(process.execPath,['-e',process.argv[1],process.argv[2],String(process.pid)],{stdio:'ignore'});child.unref();const timer=setInterval(()=>{if(fs.existsSync(process.argv[2])){clearInterval(timer);process.exit(0)}},5)";
  let group: number | undefined;
  try {
    await command(
      node,
      ["-e", leader, descendant, marker],
      root,
      childEnvironment(root),
    );
    const receipt = JSON.parse((await readOwned(marker)).toString());
    group = receipt.group;
    expect(Number.isSafeInteger(group) && Number(group) > 1).toBe(true);
    expect(() => process.kill(-Number(group), 0)).toThrow("ESRCH");
  } finally {
    if (group === undefined) {
      try {
        group = JSON.parse((await readOwned(marker)).toString()).group;
      } catch {}
    }
    if (Number.isSafeInteger(group) && Number(group) > 1) {
      try {
        process.kill(-Number(group), "SIGKILL");
      } catch (error) {
        if (
          !(error instanceof Error && "code" in error && error.code === "ESRCH")
        )
          throw error;
      }
      const deadline = performance.now() + 1500;
      let gone = false;
      while (performance.now() < deadline) {
        try {
          process.kill(-Number(group), 0);
        } catch (error) {
          if (
            error instanceof Error &&
            "code" in error &&
            error.code === "ESRCH"
          ) {
            gone = true;
            break;
          }
          throw error;
        }
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
      if (!gone) throw new Error("controlled descendant cleanup failed");
    }
  }
});

test("FC executable prerequisites reject invalid headers, architecture, platform and mode", async () => {
  const elf = Buffer.alloc(64);
  Buffer.from([0x7f, 69, 76, 70, 2, 1]).copy(elf);
  elf.writeUInt16LE(183, 18);
  const valid = await put(join(root, "fc-valid"), elf, true);
  const inputs = input();
  inputs.backend = "firecracker";
  for (const name of BINARIES) inputs.binaries[name] = valid;
  inputs.firecracker = valid;
  inputs.proxy = valid;
  await validateExecutables(inputs);
  const wrongArchitecture = Buffer.from(elf);
  wrongArchitecture.writeUInt16LE(62, 18);
  const wrongPlatform = Buffer.alloc(64);
  wrongPlatform.writeUInt32LE(0xfeedfacf);
  wrongPlatform.writeUInt32LE(0x0100000c, 4);
  const rejected: boolean[] = [];
  for (const role of ["firecracker", "proxy"] as const) {
    for (const [kind, bytes, executable] of [
      ["text", Buffer.from("correctly hashed text is not an executable"), true],
      ["architecture", wrongArchitecture, true],
      ["platform", wrongPlatform, true],
      ["mode", elf, false],
    ] as const) {
      const pin = await put(
        join(root, `fc-${role}-${kind}`),
        bytes,
        executable,
      );
      await verifyPin(pin);
      inputs[role] = pin;
      rejected.push(
        await validateExecutables(inputs).then(
          () => false,
          () => true,
        ),
      );
      inputs[role] = valid;
    }
  }
  expect(rejected).toEqual(Array(8).fill(true));
});

test("FC thin device contract uses kernel numbers and process birth", async () => {
  const { linuxDevice, procBirth, FcRuntime } = await import("./fc-runtime.ts");
  expect(linuxDevice(11008n)).toBe("43:0");
  const fields = ["S", ...Array.from({ length: 18 }, () => "0"), "123456", "0"];
  expect(procBirth(`42 (a name ) with spaces) ${fields.join(" ")}`, 42)).toBe(
    "123456",
  );
  expect(() => procBirth(`41 (name) ${fields.join(" ")}`, 42)).toThrow();
  const inventory = {
    bootId: "11111111-1111-4111-8111-111111111111",
    release: "fixed",
    pidNamespace: "pid:[1]",
    pool: poolFixture(),
  };
  const runtime = new FcRuntime(
    inventory,
    {} as import("./preflight.ts").Inputs,
    "/owned/run",
  );
  expect(runtime.deviceOptions()).toContain("b 253:* rwm");
  expect(runtime.deviceOptions()).toContain(
    "/dev/mapper/control:/dev/mapper/control:rw",
  );
  expect(runtime.environment()).toEqual({ ENGRAM_THIN_POOL: "suite" });
});

test("guest enable accepts an unobserved queued digest and verifies the ready image", async () => {
  const { create } =
    await import("../../../../orchestrator/node_modules/@bufbuild/protobuf/dist/esm/index.js");
  const {
    EnableImageResponseSchema,
    GetEnableJobResponseSchema,
    ListEnabledImagesResponseSchema,
  } =
    await import("../../../../orchestrator/src/gen/engram/app/v1/image_pb.ts");
  const { GetHostResponseSchema } =
    await import("../../../../orchestrator/src/gen/engram/app/v1/fleet_pb.ts");
  const { WhoAmIResponseSchema } =
    await import("../../../../orchestrator/src/gen/engram/app/v1/api_key_pb.ts");
  const transport =
    await import("../../../../orchestrator/scripts/mcp-stack/transport.ts");
  const { enableGuest } = await import("./main.ts");
  const directory = await mkdtemp(join(root, "enable-identity-"));
  const inputs = input();
  const opensslPath = await realpath(
    process.env.MCP_PLATFORM_TEST_OPENSSL ?? Bun.which("openssl") ?? "openssl",
  );
  inputs.binaries.openssl = {
    path: opensslPath,
    sha256: hash(await readFile(opensslPath)),
  };
  const identity = await generateIdentity(inputs, directory);
  const expectedDigest = `sha256:${"d".repeat(64)}`;
  const wrongDigest = `sha256:${"e".repeat(64)}`;
  const uri = "registry.fixture.example/guest:fixture";
  for (const scenario of [
    {
      name: "unobserved-ready",
      id: "job-1",
      queuedDigest: undefined,
      readyDigest: undefined,
      host: "correct",
      catalogDigest: expectedDigest,
      error: undefined,
    },
    {
      name: "unobserved",
      id: "job-1",
      queuedDigest: undefined,
      readyDigest: expectedDigest,
      host: "correct",
      catalogDigest: expectedDigest,
      error: undefined,
    },
    {
      name: "wrong-ready",
      id: "job-1",
      queuedDigest: undefined,
      readyDigest: wrongDigest,
      host: "correct",
      catalogDigest: expectedDigest,
      error: "enable_ready_manifest",
    },
    {
      name: "missing-id",
      id: "",
      queuedDigest: undefined,
      readyDigest: expectedDigest,
      host: "correct",
      catalogDigest: expectedDigest,
      error: "enable_manifest",
    },
    {
      name: "wrong-queued",
      id: "job-1",
      queuedDigest: wrongDigest,
      readyDigest: expectedDigest,
      host: "correct",
      catalogDigest: expectedDigest,
      error: "enable_manifest",
    },
    {
      name: "unobserved-ready-wrong-catalog",
      id: "job-1",
      queuedDigest: undefined,
      readyDigest: undefined,
      host: "correct",
      catalogDigest: wrongDigest,
      error: "enabled_image_identity",
    },
    {
      name: "wrong-materialization-host",
      id: "job-1",
      queuedDigest: undefined,
      readyDigest: expectedDigest,
      host: "wrong",
      catalogDigest: expectedDigest,
      error: "enable_actual_host",
    },
    {
      name: "missing-materialization-host",
      id: "job-1",
      queuedDigest: undefined,
      readyDigest: expectedDigest,
      host: "missing",
      catalogDigest: expectedDigest,
      error: "enable_actual_host",
    },
  ]) {
    const ctx = await context();
    ctx.runRoot = dirname(ctx.endpoints.app.ca.path);
    const admin = await transport.publicClient(ctx, {
      kind: "cookie",
      value: "fixture=cookie",
    });
    const responses = [
      create(GetEnableJobResponseSchema, {
        job: { id: "job-1", state: "materializing" },
      }),
      create(GetEnableJobResponseSchema, {
        job: {
          id: "job-1",
          state: "ready",
          manifestDigest: scenario.readyDigest,
          materializeHostId:
            scenario.host === "correct"
              ? ctx.hostId
              : scenario.host === "wrong"
                ? "00000000-0000-4000-8000-000000000002"
                : undefined,
        },
      }),
    ];
    const spies = [
      spyOn(transport, "jsonRequest").mockResolvedValue({
        status: 200,
        headers: { "set-cookie": ["fixture=cookie; HttpOnly"] },
        body: {},
      }),
      spyOn(transport, "publicClient").mockResolvedValue(admin),
      spyOn(admin.identity, "whoAmI").mockResolvedValue(
        create(WhoAmIResponseSchema, {
          email: "bootstrap@fixture.example",
          role: "admin",
          userId: "actor-1",
        }),
      ),
      spyOn(admin.fleet, "getHost").mockResolvedValue(
        create(GetHostResponseSchema, {
          host: {
            id: ctx.hostId,
            status: "ready",
            capacityTotalMib: 512n,
            readyImageDigests: [expectedDigest],
          },
        }),
      ),
      spyOn(admin.images, "enableImage").mockResolvedValue(
        create(EnableImageResponseSchema, {
          job: {
            id: scenario.id,
            state: "pending",
            manifestDigest: scenario.queuedDigest,
          },
        }),
      ),
      spyOn(admin.images, "getEnableJob").mockImplementation(async () => {
        const response = responses.shift();
        if (!response) throw new Error("unexpected enable poll");
        return response;
      }),
      spyOn(admin.images, "listEnabledImages").mockResolvedValue(
        create(ListEnabledImagesResponseSchema, {
          images: [
            {
              id: "enabled-1",
              imageUri: uri,
              manifestDigest: scenario.catalogDigest,
            },
          ],
        }),
      ),
    ];
    try {
      const enabling = enableGuest(ctx, identity, uri, expectedDigest);
      if (scenario.error) {
        await expect(enabling, scenario.name).rejects.toThrow(scenario.error);
        await expect(
          readFile(join(ctx.runRoot, "enable-observation.json")),
        ).rejects.toThrow();
      } else {
        await enabling;
        expect(
          JSON.parse(
            await readFile(
              join(ctx.runRoot, "enable-observation.json"),
              "utf8",
            ),
          ),
        ).toMatchObject({
          jobId: "job-1",
          imageId: "enabled-1",
          imageDigest: expectedDigest,
          hostId: ctx.hostId,
          state: "ready",
        });
        expect(ctx.imageId).toBe("enabled-1");
        expect(ctx.imageDigest).toBe(expectedDigest);
      }
    } finally {
      for (const spy of spies) spy.mockRestore();
    }
  }
});

test("platform validates stage selectors before resource creation and defaults to all", async () => {
  const base = [
    "--backend",
    "vz",
    "--run-root",
    "/private/run",
    "--artifacts",
    "/private/input",
  ];
  expect(cli(base).selection).toEqual({ stage: "matrix", matrix: "all" });
  expect(cli([...base, "--stage", "infrastructure"]).selection).toEqual({
    stage: "infrastructure",
  });
  const { REQUIRED_CASES } =
    await import("../../../../orchestrator/scripts/mcp-stack/manifest.ts");
  expect(cli([...base, "--matrix", REQUIRED_CASES[0]!.key]).selection).toEqual({
    stage: "matrix",
    matrix: REQUIRED_CASES[0]!.key,
  });
  for (const extra of [
    ["--stage", "unknown"],
    ["--matrix", ""],
    ["--matrix", "unknown"],
    ["--matrix", `${REQUIRED_CASES[0]!.key},${REQUIRED_CASES[0]!.key}`],
    ["--stage", "infrastructure", "--matrix", "all"],
  ])
    expect(() => cli([...base, ...extra])).toThrow();
});

test("FC inputs require a strict owned thin pool and VZ refuses that field", async () => {
  const vz = input();
  const fc = {
    ...vz,
    backend: "firecracker",
    architecture: "amd64",
    nativeProbe: undefined,
    firecracker: vz.proxy,
    images: { ...vz.images, hostAgent: vz.images.guest },
    thinPool: { name: "suite", directory: root },
  };
  expect(parseInputs(fc, "firecracker").thinPool).toEqual(fc.thinPool);
  for (const thinPool of [
    undefined,
    null,
    {},
    { ...fc.thinPool, extra: true },
    { ...fc.thinPool, name: "a".repeat(128) },
    { ...fc.thinPool, name: "../pool" },
    { ...fc.thinPool, name: "pool;true" },
    { ...fc.thinPool, directory: root + "/.." },
  ])
    expect(() => parseInputs({ ...fc, thinPool }, "firecracker")).toThrow();
  expect(() => parseInputs({ ...vz, thinPool: fc.thinPool }, "vz")).toThrow();
  expect(() => parseInputs({ ...vz, thinPool: undefined }, "vz")).toThrow();
  const dir = await mkdtemp(join(root, "pool-mode-"));
  await chmod(dir, 0o755);
  expect(() =>
    parseInputs(
      { ...fc, thinPool: { name: "suite", directory: dir } },
      "firecracker",
    ),
  ).toThrow("fc_pool_directory_owner_mode");
});

test("FC infrastructure checkpoint has no caller-selected facts and invokes the physical check", async () => {
  const { Supervisor } =
    await import("../../../../orchestrator/scripts/mcp-stack/supervisor.ts");
  const ctx = await context();
  ctx.backend = "firecracker";
  const base = {
    version: 1,
    runId: ctx.runId,
    operation: "infrastructure-ready",
  } as const;
  expect(parseOperation(base, ctx)).toEqual(base);
  for (const extra of [
    { role: "host-agent" },
    { PASS: true },
    { pool: "foreign" },
    { configurationId: "normal" },
  ])
    expect(() => parseOperation({ ...base, ...extra }, ctx)).toThrow();
  for (const fail of [false, true]) {
    let inspected = 0;
    const server = await startSupervisor(ctx, {
      status: async () => ({
        ...status(ctx),
        roles: status(ctx).roles.map((r) =>
          r.role === "host-agent" && inspected && !fail
            ? { ...r, bootId: "host-restarted" }
            : r,
        ),
      }),
      restart: async () => {},
      stop: async () => {},
      infrastructureReady: async () => {
        inspected++;
        if (fail) throw Error("physical check failed");
      },
    });
    try {
      const call = new Supervisor(ctx).infrastructureReady();
      if (fail) await expect(call).rejects.toThrow();
      else {
        const after = await call;
        expect(after.backend).toBe("firecracker");
        expect(after.roles.find((r) => r.role === "host-agent")?.bootId).toBe(
          "host-restarted",
        );
      }
      expect(inspected).toBe(1);
    } finally {
      await server.close();
    }
  }
});

for (const known of [true, false])
  test(`FC infrastructure diagnostics retain only fixed release seams (${known ? "known" : "unknown"})`, async () => {
    const { Supervisor } =
      await import("../../../../orchestrator/scripts/mcp-stack/supervisor.ts");
    const ctx = await context();
    ctx.backend = "firecracker";
    ctx.runRoot = dirname(ctx.supervisor.tokenFile);
    ctx.supervisor.socket = join(ctx.runRoot, "s");
    const seam = known
      ? "fc_unreleased_volume"
      : "fc_unreleased_volume_private_credential";
    let inspected = 0;
    const server = await startSupervisor(ctx, {
      status: async () => status(ctx),
      restart: async () => {},
      stop: async () => {},
      infrastructureReady: async () => {
        inspected++;
        throw new PlatformFailure("INVALID_CONTEXT", seam);
      },
    });
    try {
      await expect(new Supervisor(ctx).infrastructureReady()).rejects.toThrow(
        "TRANSPORT_FAILED:supervisor",
      );
      expect(inspected).toBe(1);
      const diagnostic = await readFile(
        join(ctx.runRoot, "supervisor-failure-1.json"),
        "utf8",
      );
      expect(JSON.parse(diagnostic)).toEqual({
        version: 1,
        operation: "infrastructure-ready",
        phase: "lifecycle",
        failure: known ? "INVALID_CONTEXT" : "OTHER",
        seam: known ? "fc_unreleased_volume" : "other",
      });
      expect(diagnostic).not.toContain("private_credential");
    } finally {
      await server.close();
    }
  });

for (const operation of ["prepare-disk-read", "verify-disk-read"] as const)
  test(`FC disk read control is fixed, authenticated and preserves roles (${operation})`, async () => {
    const { Supervisor } =
      await import("../../../../orchestrator/scripts/mcp-stack/supervisor.ts");
    const ctx = await context();
    const body = { version: 1, runId: ctx.runId, operation } as const;
    expect(() => parseOperation(body, ctx)).toThrow();
    ctx.backend = "firecracker";
    expect(parseOperation(body, ctx)).toEqual(body);
    for (const extra of [
      { path: "/dev/foreign" },
      { pid: 1 },
      { role: "host-agent" },
    ])
      expect(() => parseOperation({ ...body, ...extra }, ctx)).toThrow();
    let prepared = 0;
    let verified = 0;
    const server = await startSupervisor(ctx, {
      status: async () => status(ctx),
      restart: async () => {
        throw Error("unexpected restart");
      },
      stop: async () => {
        throw Error("unexpected stop");
      },
      prepareDiskRead: async () => {
        prepared++;
      },
      verifyDiskRead: async () => {
        verified++;
      },
    });
    try {
      const client = new Supervisor(ctx);
      if (operation === "prepare-disk-read") await client.prepareDiskRead();
      else await client.verifyDiskRead();
      expect([prepared, verified]).toEqual(
        operation === "prepare-disk-read" ? [1, 0] : [0, 1],
      );
    } finally {
      await server.close();
    }
  });

test("FC disk read control rejects an unrelated service restart", async () => {
  const { Supervisor } =
    await import("../../../../orchestrator/scripts/mcp-stack/supervisor.ts");
  const ctx = await context();
  ctx.backend = "firecracker";
  let changed = false;
  const server = await startSupervisor(ctx, {
    status: async () => {
      const current = status(ctx);
      if (changed)
        current.roles.find((r) => r.role === "worker")!.bootId =
          "foreign-restart";
      return current;
    },
    prepareDiskRead: async () => {
      changed = true;
    },
    restart: async () => {},
    stop: async () => {},
  });
  try {
    await expect(new Supervisor(ctx).prepareDiskRead()).rejects.toThrow(
      "TRANSPORT_FAILED:supervisor",
    );
  } finally {
    await server.close();
  }
});

for (const change of ["missing-host-restart", "other-role", "network"] as const)
  test(`FC infrastructure checkpoint rejects an invalid restart transition (${change})`, async () => {
    const { Supervisor } =
      await import("../../../../orchestrator/scripts/mcp-stack/supervisor.ts");
    const ctx = await context();
    ctx.backend = "firecracker";
    let inspected = false;
    const server = await startSupervisor(ctx, {
      status: async () => {
        const current = status(ctx);
        if (inspected) {
          if (change !== "missing-host-restart")
            current.roles.find((r) => r.role === "host-agent")!.bootId =
              "host-restarted";
          if (change === "other-role")
            current.roles.find((r) => r.role === "worker")!.bootId =
              "worker-restarted";
          if (change === "network")
            current.network.generation = "changed-network";
        }
        return current;
      },
      restart: async () => {},
      stop: async () => {},
      infrastructureReady: async () => {
        inspected = true;
      },
    });
    try {
      await expect(new Supervisor(ctx).infrastructureReady()).rejects.toThrow();
    } finally {
      await server.close();
    }
  });
