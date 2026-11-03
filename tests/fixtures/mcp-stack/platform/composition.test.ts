import { test, expect, spyOn } from "bun:test";
import {
  mkdtemp,
  mkdir,
  chmod,
  writeFile,
  readFile,
  realpath,
  rm,
} from "node:fs/promises";
import { createServer } from "node:net";
import { join } from "node:path";
import { Engine } from "./engine.ts";
import { hash, PlatformFailure } from "./owned.ts";
import { BINARIES, IMAGES, parseInputs, type Inputs } from "./preflight.ts";

test("engine operations pin socket/config and exclude parent credentials", async () => {
  const root = await mkdtemp(
    join(process.cwd(), "var/mcp-platform-composition/engine-"),
  );
  const config = join(root, "config");
  await mkdir(config, { mode: 0o700 });
  const socket = join(root, "engine.sock");
  const server = createServer();
  await new Promise<void>((r) => server.listen(socket, r));
  const script = join(root, "docker");
  const output = join(root, "args.json");
  await writeFile(
    script,
    `#!/bin/sh\nexec ${JSON.stringify(process.execPath)} -e 'require("fs").writeFileSync(${JSON.stringify(output)},JSON.stringify({args:process.argv.slice(1),env:process.env}));process.stdout.write(JSON.stringify({ID:"engine-identity-123"}))' -- "$@"\n`,
    { mode: 0o700 },
  );
  const inputs = {
    engineSocket: socket,
    engineConfig: config,
    engineId: "engine-identity-123",
    binaries: {
      docker: { path: script, sha256: hash(await readFile(script)) },
    },
  } as Inputs;
  const old = process.env.AWS_SECRET_ACCESS_KEY;
  process.env.AWS_SECRET_ACCESS_KEY = "forbidden-parent-value";
  try {
    const engine = new Engine(inputs, root, "mcp-123456789012345678901234");
    await engine.call(["info", "--format", "{{json .}}"]);
    const got = JSON.parse(await readFile(output, "utf8"));
    expect(got.args).toEqual([
      "--config",
      config,
      "--host",
      `unix://${socket}`,
      "info",
      "--format",
      "{{json .}}",
    ]);
    expect(got.env.AWS_SECRET_ACCESS_KEY).toBeUndefined();
    expect(got.env.HOME).toBeUndefined();
    expect(got.env.DOCKER_HOST).toBeUndefined();
    await writeFile(join(config, "config.json"), "{}");
    await expect(engine.call(["info"])).rejects.toThrow(
      "engine_config_not_empty",
    );
  } finally {
    if (old === undefined) delete process.env.AWS_SECRET_ACCESS_KEY;
    else process.env.AWS_SECRET_ACCESS_KEY = old;
    await new Promise<void>((r) => server.close(() => r()));
    await rm(root, { recursive: true });
  }
});

test("guest manifest identity is mandatory and cannot alias the config id by omission", async () => {
  const { parseInputs, BINARIES, IMAGES } = await import("./preflight.ts");
  const pin = { path: "/owned/binary", sha256: "a".repeat(64) };
  const value = {
    version: 1,
    backend: "vz",
    proxy: pin,
    nativeProbe: pin,
    architecture: "arm64",
    sourceCommit: "b".repeat(40),
    engineSocket: "/owned/socket",
    engineConfig: "/owned/config",
    engineId: "engine-identity-123",
    registryPort: 32000,
    subnet: "10.42.3.0/24",
    nativeHostIp: "10.42.4.1",
    binaries: Object.fromEntries(BINARIES.map((k) => [k, pin])),
    images: Object.fromEntries(
      IMAGES.map((k) => [
        k,
        {
          archive: pin,
          id: `sha256:${"c".repeat(64)}`,
          manifestDigest: `sha256:${"d".repeat(64)}`,
        },
      ]),
    ),
  };
  expect(parseInputs(value, "vz").images.guest.manifestDigest).toBe(
    `sha256:${"d".repeat(64)}`,
  );
  expect(() =>
    parseInputs(
      {
        ...value,
        images: {
          ...value.images,
          guest: { archive: pin, id: `sha256:${"c".repeat(64)}` },
        },
      },
      "vz",
    ),
  ).toThrow();
  expect(() =>
    parseInputs(
      { ...value, images: { ...value.images, hostAgent: value.images.guest } },
      "vz",
    ),
  ).toThrow();
});

test("native ownership rejects foreign executables, UID and incomplete birth identity", async () => {
  const { parseNativeIdentities } = await import("./launch.ts");
  const base = {
    pid: 1234,
    uid: process.getuid!(),
    parent: 12,
    group: 12,
    seconds: "1730000000",
    microseconds: "123456",
    executable: "/owned/run/assets/hostAgent",
  };
  expect(parseNativeIdentities([base], "/owned/run")[0]?.microseconds).toBe(
    "123456",
  );
  for (const change of [
    { uid: base.uid + 1 },
    { microseconds: "1000000" },
    { seconds: 1730000000 },
    { executable: "/foreign/hostAgent" },
    { executable: "/owned/run/host/../../foreign/netstack" },
    { pid: [1234] },
  ])
    expect(() =>
      parseNativeIdentities([{ ...base, ...change }], "/owned/run"),
    ).toThrow();
});

test("cancelled engine control runs no command and drains its control lease", async () => {
  const engine = new Engine(
    {} as Inputs,
    "/owned/run",
    "mcp-123456789012345678901234",
  );
  const abort = new AbortController();
  abort.abort();
  let called = false;
  await expect(
    engine.controlled(abort.signal, async () => {
      called = true;
    }),
  ).rejects.toThrow();
  expect(called).toBe(false);
  await engine.controlled(new AbortController().signal, async () => {
    called = true;
  });
  expect(called).toBe(true);
});

function inspectedContainer(imageRole: (typeof IMAGES)[number] = "worker") {
  const pin = { path: "/owned/binary", sha256: "a".repeat(64) };
  const inputs = parseInputs(
    {
      version: 1,
      backend: "vz",
      architecture: "arm64",
      sourceCommit: "b".repeat(40),
      engineSocket: "/owned/socket",
      engineConfig: "/owned/config",
      engineId: "engine-identity-123",
      registryPort: 32000,
      subnet: "10.42.3.0/24",
      nativeHostIp: "10.42.4.1",
      proxy: pin,
      nativeProbe: pin,
      binaries: Object.fromEntries(BINARIES.map((role) => [role, pin])),
      images: Object.fromEntries(
        IMAGES.map((role, index) => [
          role,
          {
            archive: pin,
            id: `sha256:${index.toString(16).repeat(64)}`,
            manifestDigest: `sha256:${"d".repeat(64)}`,
          },
        ]),
      ),
    },
    "vz",
  );
  const id = "a".repeat(64),
    image = inputs.images[imageRole].id,
    runId = "mcp-123456789012345678901234";
  const state = {
    Id: id,
    Image: image,
    Config: {
      Labels: { "engrams.mcp.run": runId },
      User: "1000:1000",
      Env: ["PATH=/usr/bin:/bin"],
      Entrypoint: ["/bin/sleep"],
      Cmd: ["infinity"],
    },
    HostConfig: {
      RestartPolicy: { Name: "no" },
      Privileged: false,
      ReadonlyRootfs: true,
      PidsLimit: 128,
      Memory: 805306368,
      NanoCpus: 2000000000,
      NetworkMode: "none",
      PidMode: "",
      IpcMode: "private",
      CapDrop: ["ALL"],
      CapAdd: Array<string>(),
      SecurityOpt: ["no-new-privileges"],
      Binds: [],
      VolumesFrom: [],
      Devices: [],
      DeviceRequests: [],
      Tmpfs: { "/tmp": "rw,nosuid,nodev,size=4m" },
    },
    Mounts: [],
    State: { Running: true, Pid: 42 },
  };
  const imageState = {
    Id: image,
    Config: {
      Env: ["PATH=/usr/bin:/bin"],
      Entrypoint: ["/bin/sleep"],
      Cmd: ["infinity"],
    },
  };
  class Inspector extends Engine {
    override async call(args: string[]): Promise<Buffer> {
      if (args[0] === "create") return Buffer.from(id);
      if (args[0] === "container" && args[1] === "inspect")
        return Buffer.from(JSON.stringify([state]));
      if (args[0] === "image") return Buffer.from(JSON.stringify([imageState]));
      if (args[0] === "start") return Buffer.from(id);
      throw new Error("unexpected fixed test operation");
    }
  }
  const engine = new Inspector(inputs, "/owned/run", runId);
  const options = [
    "--network",
    "none",
    "--user",
    "1000:1000",
    "--read-only",
    "--tmpfs",
    "/tmp:rw,nosuid,nodev,size=4m",
  ];
  return { engine, state, imageState, id, image, options };
}

const pinnedImageDefaults = [
  {
    imageRole: "guard",
    entry: "HOME=/tmp",
    wrong: ["HOME=/root", "HOME=/private/home"],
  },
  {
    imageRole: "orchestrator",
    entry: "BUN_INSTALL_BIN=/usr/local/bin",
    wrong: ["BUN_INSTALL_BIN=/tmp/bin"],
  },
  {
    imageRole: "orchestrator",
    entry: "BUN_RUNTIME_TRANSPILER_CACHE_PATH=0",
    wrong: ["BUN_RUNTIME_TRANSPILER_CACHE_PATH=/tmp/cache", "BUN_RUNTIME_TRANSPILER_CACHE_PATH=1"],
  },
  {
    imageRole: "dns",
    entry: "SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt",
    wrong: ["SSL_CERT_FILE=/tmp/ca.pem"],
  },
] satisfies {
  imageRole: (typeof IMAGES)[number];
  entry: string;
  wrong: string[];
}[];

test.each(pinnedImageDefaults)(
  "pinned image default $entry passes its exact image audit",
  async ({ imageRole, entry }) => {
    const { engine, state, imageState, image, options, id } = inspectedContainer(imageRole);
    imageState.Config.Env.push(entry);
    state.Config.Env.push(entry);
    await engine.create("probe", image, options, ["infinity"], "/bin/sleep");
    await expect(engine.audit(id)).resolves.toBeDefined();
    state.Config.Env.push("DATABASE_URL=forbidden");
    await expect(engine.audit(id)).rejects.toThrow("container_environment");
    state.Config.Env.pop();
    state.Config.Env[1] = entry + "-changed";
    await expect(engine.audit(id)).rejects.toThrow("container_environment");
  },
);

test.each(pinnedImageDefaults)(
  "pinned image default $entry rejects other images and values",
  async ({ imageRole, entry, wrong }) => {
    for (const [role, value] of [
      ["worker", entry],
      ...wrong.map((value): [(typeof IMAGES)[number], string] => [imageRole, value]),
      [imageRole, "AWS_SECRET_ACCESS_KEY=forbidden"],
      [imageRole, "HTTPS_PROXY=http://proxy.invalid"],
    ] satisfies [(typeof IMAGES)[number], string][]) {
      const { engine, state, imageState, image, options } = inspectedContainer(role);
      imageState.Config.Env.push(value);
      state.Config.Env.push(value);
      if (imageRole === "dns") {
        state.HostConfig.CapAdd = ["NET_BIND_SERVICE"];
        options.push("--cap-add", "NET_BIND_SERVICE");
      }
      await expect(
        engine.create(imageRole, image, options, ["infinity"], "/bin/sleep"),
      ).rejects.toThrow("image_environment");
    }
  },
);

test.each(["tool-check", "pack-claude", "pack-codex", "probe", "host-tls-probe"])(
  "pinned image default HOME is available to guard reuse %s",
  async (role) => {
    const { engine, state, imageState, image, options } = inspectedContainer("guard");
    imageState.Config.Env.push("HOME=/tmp");
    state.Config.Env.push("HOME=/tmp");
    await engine.create(role, image, options, ["infinity"], "/bin/sleep");
  },
);

test.each(["dns", "denied-dns"])(
  "pinned image default certificate is available to DNS reuse %s",
  async (role) => {
    const { engine, state, imageState, image, options } = inspectedContainer("dns");
    imageState.Config.Env.push("SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt");
    state.Config.Env.push("SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt");
    state.HostConfig.CapAdd = ["NET_BIND_SERVICE"];
    await engine.create(
      role,
      image,
      [...options, "--cap-add", "NET_BIND_SERVICE"],
      ["infinity"],
      "/bin/sleep",
    );
  },
);

test("pinned image defaults allow both Bun entries together but preserve image identity", async () => {
  const { engine, state, imageState, image, options, id } = inspectedContainer("orchestrator");
  const defaults = pinnedImageDefaults
    .filter(({ imageRole }) => imageRole === "orchestrator")
    .map(({ entry }) => entry);
  imageState.Config.Env.push(...defaults);
  state.Config.Env.push(...defaults);
  await engine.create("probe", image, options, ["infinity"], "/bin/sleep");
  imageState.Id = "sha256:" + "f".repeat(64);
  await expect(engine.audit(id)).rejects.toThrow("container_image");
});

test("resolver-stage compares equivalent capability names exactly", async () => {
  const { engine, state, id, image, options } = inspectedContainer();
  const requested = ["CHOWN", "DAC_OVERRIDE", "FOWNER"];
  state.HostConfig.CapAdd = requested.map((capability) => "CAP_" + capability);
  await engine.create(
    "resolver-stage",
    image,
    [
      ...options,
      ...requested.flatMap((capability) => ["--cap-add", capability]),
    ],
    ["infinity"],
    "/bin/sleep",
  );
  state.HostConfig.CapAdd = requested;
  await expect(engine.audit(id)).resolves.toBeDefined();
  for (const capabilities of [
    ["CAP_CHOWN", "CAP_DAC_OVERRIDE"],
    ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_NET_ADMIN"],
    [...requested, "CAP_NET_ADMIN"],
    [...requested, "CAP_FOWNER"],
    ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER", "CAP_FOWNER"],
    ["CAP_CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER"],
  ]) {
    state.HostConfig.CapAdd = capabilities;
    await expect(engine.audit(id)).rejects.toThrow("container_capabilities");
  }
  state.HostConfig.CapAdd = requested.map((capability) => "CAP_" + capability);
  for (const dropped of [
    [],
    ["CAP_ALL"],
    ["ALL", "NET_ADMIN"],
    ["ALL", "ALL"],
  ]) {
    state.HostConfig.CapDrop = dropped;
    await expect(engine.audit(id)).rejects.toThrow("container_capabilities");
  }
  state.HostConfig.CapDrop = ["ALL"];
  for (const security of [
    [],
    ["no-new-privileges:true"],
    ["no-new-privileges", "seccomp=unconfined"],
  ]) {
    state.HostConfig.SecurityOpt = security;
    await expect(engine.audit(id)).rejects.toThrow("container_capabilities");
  }
  state.HostConfig.SecurityOpt = ["no-new-privileges"];
  await expect(engine.audit(id)).resolves.toBeDefined();
});

test.each(["dns", "denied-dns"])(
  "%s requires exactly NET_BIND_SERVICE in its effective audit",
  async (role) => {
    const { engine, state, id, image, options } = inspectedContainer();
    state.HostConfig.CapAdd = ["CAP_NET_BIND_SERVICE"];
    await engine.create(
      role,
      image,
      [...options, "--cap-add", "NET_BIND_SERVICE"],
      ["infinity"],
      "/bin/sleep",
    );
    state.HostConfig.CapAdd = ["NET_BIND_SERVICE"];
    await expect(engine.audit(id)).resolves.toBeDefined();
    for (const capabilities of [
      [],
      ["NET_ADMIN"],
      ["NET_BIND_SERVICE", "NET_ADMIN"],
      ["NET_BIND_SERVICE", "NET_BIND_SERVICE"],
      ["CAP_NET_BIND_SERVICE", "CAP_NET_BIND_SERVICE"],
      ["NET_BIND_SERVICE", "CAP_NET_BIND_SERVICE"],
      ["CAP_CAP_NET_BIND_SERVICE"],
    ]) {
      state.HostConfig.CapAdd = capabilities;
      await expect(engine.audit(id)).rejects.toThrow("container_capabilities");
    }
    const missing = inspectedContainer();
    await expect(
      missing.engine.create(
        role,
        missing.image,
        missing.options,
        ["infinity"],
        "/bin/sleep",
      ),
    ).rejects.toThrow("container_capabilities");
    const extra = inspectedContainer();
    extra.state.HostConfig.CapAdd = ["NET_BIND_SERVICE", "NET_ADMIN"];
    await expect(
      extra.engine.create(
        role,
        extra.image,
        [
          ...extra.options,
          "--cap-add",
          "NET_BIND_SERVICE",
          "--cap-add",
          "NET_ADMIN",
        ],
        ["infinity"],
        "/bin/sleep",
      ),
    ).rejects.toThrow("container_capabilities");
  },
);

test.each([
  "worker",
  "orchestrator",
  "coordinator",
  "probe",
  "dns-other",
  "denied-dns-other",
])("%s cannot request the fixture DNS capability", async (role) => {
  const { engine, state, image, options } = inspectedContainer();
  state.HostConfig.CapAdd = ["NET_BIND_SERVICE"];
  await expect(
    engine.create(
      role,
      image,
      [...options, "--cap-add", "NET_BIND_SERVICE"],
      ["infinity"],
      "/bin/sleep",
    ),
  ).rejects.toThrow("container_capabilities");
});

test("effective container audit rejects added authority and environment", async () => {
  const { engine, state, id, image } = inspectedContainer();
  await engine.create(
    "probe",
    image,
    [
      "--network",
      "none",
      "--user",
      "1000:1000",
      "--read-only",
      "--tmpfs",
      "/tmp:rw,nosuid,nodev,size=4m",
    ],
    ["infinity"],
    "/bin/sleep",
  );
  state.Config.Env.push("DATABASE_URL=forbidden");
  await expect(engine.audit(id)).rejects.toThrow("container_environment");
  state.Config.Env.pop();
  state.HostConfig.Privileged = true;
  await expect(engine.audit(id)).rejects.toThrow("container_resources");
  state.HostConfig.Privileged = false;
  state.Config.User = "0:0";
  await expect(engine.audit(id)).rejects.toThrow("container_namespace");
  state.Config.User = "1000:1000";
  Reflect.set(state.HostConfig, "PortBindings", {
    "8443/tcp": [{ HostIp: "0.0.0.0", HostPort: "8443" }],
  });
  await expect(engine.audit(id)).rejects.toThrow("container_exposure");
  Reflect.deleteProperty(state.HostConfig, "PortBindings");
  state.HostConfig.PidMode = "container:foreign";
  await expect(engine.audit(id)).rejects.toThrow("container_namespace");
  state.HostConfig.PidMode = "host";
  await expect(engine.audit(id)).rejects.toThrow("container_namespace");
  state.HostConfig.PidMode = "";
  Reflect.set(state.HostConfig, "Sysctls", { "net.ipv4.ip_forward": "1" });
  await expect(engine.audit(id)).rejects.toThrow("container_exposure");
  Reflect.deleteProperty(state.HostConfig, "Sysctls");
  Reflect.set(state.HostConfig, "CgroupnsMode", "host");
  await expect(engine.audit(id)).rejects.toThrow("container_namespace");
  Reflect.deleteProperty(state.HostConfig, "CgroupnsMode");
  await expect(engine.audit(id)).resolves.toBeDefined();
});

test("entrypoint rejects a foreign backend before creating output or invoking a matrix", async () => {
  const { main, matrixArguments } = await import("./main.ts");
  const { BINARIES, IMAGES } = await import("./preflight.ts");
  const root = await mkdtemp(
    join(process.cwd(), "var/mcp-platform-composition/entry-"),
  );
  try {
    const output = join(root, "new-run"),
      manifest = join(root, "manifest.json"),
      pin = { path: "/owned/binary", sha256: "a".repeat(64) };
    await writeFile(
      manifest,
      JSON.stringify({
        version: 1,
        backend: "vz",
        architecture: "arm64",
        proxy: pin,
        nativeProbe: pin,
        sourceCommit: "b".repeat(40),
        engineSocket: "/owned/engine.sock",
        engineConfig: "/owned/config",
        engineId: "engine-identity-123",
        registryPort: 32000,
        subnet: "10.42.3.0/24",
        nativeHostIp: "10.42.4.1",
        binaries: Object.fromEntries(BINARIES.map((k) => [k, pin])),
        images: Object.fromEntries(
          IMAGES.map((k) => [
            k,
            {
              archive: pin,
              id: "sha256:" + "c".repeat(64),
              manifestDigest: "sha256:" + "d".repeat(64),
            },
          ]),
        ),
      }),
      { mode: 0o600 },
    );
    await expect(
      main([
        "--backend",
        "firecracker",
        "--run-root",
        output,
        "--artifacts",
        manifest,
      ]),
    ).rejects.toThrow("platform_identity");
    await expect(readFile(join(output, "context.json"))).rejects.toThrow();
    expect(matrixArguments("/owned/context.json").slice(1)).toEqual([
      "--context",
      "/owned/context.json",
      "--matrix",
      "all",
    ]);
  } finally {
    await rm(root, { recursive: true });
  }
});

for (const failedLookup of [false, true]) {
  test(`real startup prefix checks DNS after dependencies and stops on failure (${failedLookup})`, async () => {
    const { bootPlatform } = await import("./main.ts");
    const { Stack } = await import("./launch.ts");
    const { FixtureObserver } =
      await import("../../../../orchestrator/scripts/mcp-stack/observer.ts");
    const inputs = {
      backend: "vz",
      images: { fixture: { id: "fixture-image" } },
    } as Inputs;
    const ctx = {
      protocolFixtures: {
        modern: { runRoot: "/owned/modern", configuration: { path: "/owned/modern/config" } },
        legacy: { runRoot: "/owned/legacy", configuration: { path: "/owned/legacy/config" } },
      },
    } as import("../../../../orchestrator/scripts/mcp-stack/context.ts").Context;
    const steps: string[] = [];
    const step = async (name: string) => {
      steps.push(name);
    };
    const engine = new Engine(inputs, "/owned", "mcp-123456789012345678901234");
    engine.reserveRegistryPort = () => step("registry-port");
    engine.reserveHostPort = () => step("host-port");
    engine.qualifyTools = () => step("tools");
    engine.network = () => step("network");
    engine.networkOptions = () => [];
    engine.create = async (role) => {
      steps.push(role);
      return role;
    };
    engine.nativeDns = async () => {
      steps.push("native-dns");
      if (!steps.includes("dependencies"))
        throw new PlatformFailure("NEEDS_CONTEXT", "dns_before_dependencies");
      if (failedLookup) throw new PlatformFailure("NEEDS_CONTEXT", "native_dns_lookup");
    };
    engine.guards = () => step("guards");
    engine.verifyNetwork = async (probes) => {
      steps.push(`verify-network:${probes}`);
      return {
        generation: "test-generation",
        enforcementSha256: "a".repeat(64),
        fixtureOnlyEgress: true,
        externalDnsBlocked: true,
        roles: ["orchestrator", "coordinator", "worker"],
      };
    };
    const stack = new Stack(
      inputs,
      "/owned",
      {} as ConstructorParameters<typeof Stack>[2],
      {} as ConstructorParameters<typeof Stack>[3],
      {} as ConstructorParameters<typeof Stack>[4],
      engine,
    );
    stack.dependencies = () => step("dependencies");
    stack.edges = () => step("edges");
    stack.startRole = (_ctx, role) => step(`start:${role}`);
    stack.readyRole = (_ctx, role) => step(`ready:${role}`);
    stack.migrateOrchestrator = () => step("migrate");
    // Replace only the native I/O boundary; retain the real fixture launch and DNS gate.
    const ready = spyOn(FixtureObserver.prototype, "ready").mockImplementation(() =>
      step("observer"),
    );
    try {
      const boot = bootPlatform(engine, { path: "/owned/ca", sha256: "a".repeat(64) }, async () => {
        steps.push("prepare");
        return { stack, ctx };
      });
      const prefix = [
        "registry-port",
        "host-port",
        "tools",
        "prepare",
        "network",
        "dependencies",
        "fixture-modern",
        "fixture-legacy",
        "native-dns",
      ];
      if (failedLookup) {
        await expect(boot).rejects.toMatchObject({
          code: "NEEDS_CONTEXT",
          seam: "native_dns_lookup",
        });
        expect(steps).toEqual(prefix);
      } else {
        await expect(boot).resolves.toBe(ctx);
        expect(steps).toEqual([
          ...prefix,
          "observer",
          "observer",
          "guards",
          "verify-network:true",
          "edges",
          "start:worker",
          "ready:worker",
          "start:coordinator",
          "ready:coordinator",
          "migrate",
          "start:orchestrator",
          "ready:orchestrator",
          "start:host-agent",
          "ready:host-agent",
        ]);
      }
    } finally {
      ready.mockRestore();
    }
  });
}

test("native DNS accepts only the expected addresses for every fixture name", async () => {
  const engine = new Engine(
    { subnet: "10.42.3.0/24", nativeHostIp: "10.42.4.1" } as Inputs,
    "/owned",
    "mcp-123456789012345678901234",
  );
  const records: Record<string, string> = {
    "postgres.fixture.example": "10.42.3.10",
    "app.fixture.example": "10.42.3.20",
    "coordinator.fixture.example": "10.42.3.21",
    "operator.fixture.example": "10.42.3.21",
    "control.fixture.example": "10.42.3.21",
    "worker.fixture.example": "10.42.3.22",
    "host.fixture.example": "10.42.4.1",
    "resource.modern.fixture.example": "10.42.3.30",
    "issuer.modern.fixture.example": "10.42.3.30",
    "authorization.modern.fixture.example": "10.42.3.30",
    "token.modern.fixture.example": "10.42.3.30",
    "observer.modern.fixture.example": "10.42.3.30",
    "resource.legacy.fixture.example": "10.42.3.31",
    "issuer.legacy.fixture.example": "10.42.3.31",
    "authorization.legacy.fixture.example": "10.42.3.31",
    "token.legacy.fixture.example": "10.42.3.31",
    "observer.legacy.fixture.example": "10.42.3.31",
  };
  const names: string[] = [];
  await engine.nativeDns(async (name, options) => {
    expect(options).toEqual({ all: true });
    expect(records[name]).toBeDefined();
    names.push(name);
    return [{ address: records[name]!, family: 4 }];
  });
  expect(names).toEqual(Object.keys(records));
});

for (const actual of [
  [],
  [{ address: "10.42.3.11", family: 4 }],
  [
    { address: "10.42.3.10", family: 4 },
    { address: "10.42.3.11", family: 4 },
  ],
]) {
  test(`native DNS rejects empty or wrong successful answers (${actual.length})`, async () => {
    const engine = new Engine(
      { subnet: "10.42.3.0/24", nativeHostIp: "10.42.4.1" } as Inputs,
      "/owned",
      "mcp-123456789012345678901234",
    );
    await expect(engine.nativeDns(async () => actual)).rejects.toMatchObject({
      code: "NEEDS_CONTEXT",
      seam: "native_dns_route",
    });
  });
}

test("native DNS lookup rejection exposes only the static safe seam", async () => {
  const engine = new Engine(
    { subnet: "10.42.3.0/24", nativeHostIp: "10.42.4.1" } as Inputs,
    "/owned",
    "mcp-123456789012345678901234",
  );
  let failure: unknown;
  try {
    await engine.nativeDns(async () => {
      throw new Error("secret-marker lookup postgres.fixture.example at 10.42.3.10");
    });
  } catch (error) {
    failure = error;
  }
  expect(failure).toBeInstanceOf(PlatformFailure);
  expect(failure).toMatchObject({ code: "NEEDS_CONTEXT", seam: "native_dns_lookup" });
  expect(String(failure)).toBe("Error: NEEDS_CONTEXT:native_dns_lookup");
  expect(JSON.stringify(failure)).not.toMatch(/secret-marker|fixture\.example|10\.42\.3\.10/);
  expect(failure).not.toHaveProperty("cause");
});

test("fixed boot graph waits for worker, coordinator and real migration before app and host", async () => {
  const { bootApplications } = await import("./main.ts");
  const ctx =
    {} as import("../../../../orchestrator/scripts/mcp-stack/context.ts").Context;
  const steps: string[] = [];
  const lifecycle = {
    startRole: async (_ctx: typeof ctx, role: string, variant: string) => {
      steps.push(`start:${role}:${variant}`);
    },
    readyRole: async (_ctx: typeof ctx, role: string) => {
      steps.push(`ready:${role}`);
    },
    migrateOrchestrator: async () => {
      steps.push("migrate");
    },
  };
  await bootApplications(lifecycle, ctx);
  expect(steps).toEqual([
    "start:worker:normal",
    "ready:worker",
    "start:coordinator:normal",
    "ready:coordinator",
    "migrate",
    "start:orchestrator:normal",
    "ready:orchestrator",
    "start:host-agent:normal",
    "ready:host-agent",
  ]);
  steps.length = 0;
  await expect(
    bootApplications(
      {
        ...lifecycle,
        migrateOrchestrator: async () => {
          throw new Error("real migration failed");
        },
      },
      ctx,
    ),
  ).rejects.toThrow("real migration failed");
  expect(steps).toEqual([
    "start:worker:normal",
    "ready:worker",
    "start:coordinator:normal",
    "ready:coordinator",
  ]);
});

test("FC bind stays in its private namespace while VZ binds the verified native address", async () => {
  const { hostListenAddress } = await import("./launch.ts");
  const { dnsRecords, dnsHosts, address, rules } = await import("./network.ts");
  const inputs = {
    backend: "firecracker" as const,
    nativeHostIp: "10.42.4.1",
    subnet: "10.42.3.0/24",
  };
  expect(hostListenAddress(inputs)).toBe("0.0.0.0:9101");
  expect(hostListenAddress({ ...inputs, backend: "vz" })).toBe(
    "10.42.4.1:9101",
  );
  expect(dnsRecords(inputs)["postgres.fixture.example"]).toBe("10.42.3.10");
  expect(dnsHosts(inputs)).toContain("10.42.3.10 postgres.fixture.example\n");
  expect(JSON.stringify(rules(inputs, "worker"))).not.toContain(
    address(inputs, "postgres"),
  );
});

test("abort terminates an in-flight pinned CLI and releases the control lease", async () => {
  const root = await mkdtemp(
    join(process.cwd(), "var/mcp-platform-composition/cancel-"),
  );
  const config = join(root, "config");
  await mkdir(config, { mode: 0o700 });
  const socket = join(root, "engine.sock");
  const server = createServer();
  await new Promise<void>((resolve) => server.listen(socket, resolve));
  const script = join(root, "docker"),
    started = join(root, "started");
  await writeFile(
    script,
    `#!/bin/sh\nexec ${JSON.stringify(process.execPath)} -e 'require("fs").writeFileSync(${JSON.stringify(started)},String(process.pid));setInterval(()=>{},100)'\n`,
    { mode: 0o700 },
  );
  const inputs = {
    engineSocket: socket,
    engineConfig: config,
    binaries: {
      docker: { path: script, sha256: hash(await readFile(script)) },
    },
  } as Inputs;
  const controller = new AbortController();
  try {
    const engine = new Engine(inputs, root, "mcp-123456789012345678901234");
    const running = engine.controlled(controller.signal, () =>
      engine.call(["info"], undefined, 30000),
    );
    const outcome = running.then(
      () => false,
      () => true,
    );
    const deadline = performance.now() + 2000;
    while (!(await readFile(started).catch(() => undefined))) {
      if (performance.now() >= deadline)
        throw new Error("controlled child did not start");
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    const pid = Number(await readFile(started, "utf8"));
    controller.abort();
    expect(await outcome).toBe(true);
    expect(() => process.kill(pid, 0)).toThrow();
    await expect(
      engine.controlled(new AbortController().signal, async () => "drained"),
    ).resolves.toBe("drained");
  } finally {
    controller.abort();
    await new Promise<void>((resolve) => server.close(() => resolve()));
    await rm(root, { recursive: true });
  }
});

test("cleanup retains a failure receipt and refuses to delete a substituted network", async () => {
  const root = await mkdtemp(
    join(process.cwd(), "var/mcp-platform-composition/cleanup-"),
  );
  const runId = "mcp-123456789012345678901234",
    id = "c".repeat(64);
  const operations: string[][] = [];
  let foreign = false;
  class CleanupEngine extends Engine {
    override async project(): Promise<string> {
      return `${runId}-resolver`;
    }
    override async call(args: string[]): Promise<Buffer> {
      operations.push(args);
      if (args[0] === "network" && args[1] === "ls") return Buffer.from("");
      if (args[0] === "network" && args[1] === "create") return Buffer.from(id);
      if (args[0] === "network" && args[1] === "inspect")
        return Buffer.from(
          JSON.stringify([
            {
              Id: id,
              Internal: true,
              Driver: "bridge",
              Labels: { "engrams.mcp.run": foreign ? "foreign" : runId },
              IPAM: {
                Config: [{ Subnet: "10.42.3.0/24", Gateway: "10.42.3.1" }],
              },
              Containers: {},
            },
          ]),
        );
      if (args[0] === "ps") return Buffer.from("");
      throw new Error("unexpected cleanup command");
    }
  }
  try {
    const engine = new CleanupEngine(
      { subnet: "10.42.3.0/24" } as Inputs,
      root,
      runId,
    );
    await engine.network();
    foreign = true;
    await expect(engine.cleanup()).rejects.toThrow("engine_cleanup");
    expect(
      operations.some((args) => args[0] === "network" && args[1] === "rm"),
    ).toBe(false);
    const receipt = JSON.parse(
      await readFile(join(root, "cleanup.json"), "utf8"),
    );
    expect(receipt.success).toBe(false);
    expect(receipt.networkRemaining).toBe(true);
  } finally {
    await rm(root, { recursive: true });
  }
});

test("FC launch uses the declared configuration, private namespace and original image entrypoint", async () => {
  const { generateIdentity } = await import("./identity.ts");
  const { generateConfiguration } = await import("./configuration.ts");
  const { parseInputs, BINARIES, IMAGES } = await import("./preflight.ts");
  const { Stack } = await import("./launch.ts");
  const { put } = await import("./owned.ts");
  const root = await mkdtemp(
    join(process.cwd(), "var/mcp-platform-composition/fc-plan-"),
  );
  try {
    const pin = await put(join(root, "binary"), "controlled test artifact");
    const openssl = await realpath(
      process.env.MCP_PLATFORM_TEST_OPENSSL ??
        Bun.which("openssl") ??
        "openssl",
    );
    const image = {
      archive: pin,
      id: `sha256:${"c".repeat(64)}`,
      manifestDigest: `sha256:${"d".repeat(64)}`,
    };
    const inputs = parseInputs(
      {
        version: 1,
        backend: "firecracker",
        architecture: "arm64",
        sourceCommit: "b".repeat(40),
        engineSocket: "/owned/engine.sock",
        engineConfig: "/owned/config",
        engineId: "engine-identity-123",
        registryPort: 32000,
        subnet: "10.42.3.0/24",
        nativeHostIp: "10.42.4.1",
        proxy: pin,
        firecracker: pin,
        binaries: {
          ...Object.fromEntries(BINARIES.map((key) => [key, pin])),
          openssl: { path: openssl, sha256: hash(await readFile(openssl)) },
        },
        images: {
          ...Object.fromEntries(IMAGES.map((key) => [key, image])),
          hostAgent: image,
        },
      },
      "firecracker",
    );
    const identity = await generateIdentity(inputs, root),
      configuration = await generateConfiguration(inputs, root, identity);
    const artifacts = {
      coordinator: pin,
      orchestrator: pin,
      worker: pin,
      fixture: pin,
      guestImage: pin,
      hostAgent: pin,
      kernel: pin,
      helper: pin,
      claudeHarness: pin,
      codexHarness: pin,
      modelCli: pin,
      claudeBundle: pin,
      codexBundle: pin,
    };
    const assets = {
      binaries: inputs.binaries,
      proxy: pin,
      nativeProbe: undefined,
      firecracker: pin,
      artifacts,
      bundles: join(root, "bundles"),
      stamp: {},
    };
    const launches: {
      role: string;
      image: string;
      options: string[];
      args: string[] | undefined;
      entrypoint: string | undefined;
    }[] = [];
    class LaunchEngine extends Engine {
      override networkName() {
        return "owned-network";
      }
      override networkOptions() {
        return ["--network", this.networkName()];
      }
      override async project(role: string): Promise<string> {
        return `${this.runId}-${role}`;
      }
      override async call(args: string[]): Promise<Buffer> {
        if (args[0] === "exec" && args[2] === "/usr/bin/dig")
          return Buffer.from("10.42.3.40\n");
        if (args[0] === "exec") return Buffer.from("");
        throw new Error("unexpected dependency command");
      }
      override async remove() {}
      override async create(
        role: string,
        image: string,
        options: string[],
        args?: string[],
        entrypoint?: string,
      ): Promise<string> {
        launches.push({ role, image, options, args, entrypoint });
        return "a".repeat(64);
      }
    }
    const engine = new LaunchEngine(inputs, root, identity.runId, {
      bootId: "11111111-1111-4111-8111-111111111111",
      readSectors: { "/dev/nbd0": "0", "/dev/nbd1": "0" },
      release: "test-kernel",
      pidNamespace: "pid:[1]",
      slots: [
        { path: "/dev/nbd0", ino: "41", rdev: "11008", device: "43:0", uid: 0 },
        { path: "/dev/nbd1", ino: "42", rdev: "11009", device: "43:1", uid: 0 },
      ],
    });
    engine.guardIds.set("coordinator", "e".repeat(64));
    const stack = new Stack(
      inputs,
      root,
      identity,
      assets,
      configuration,
      engine,
    );
    const ctx = await stack.prepare();
    await stack.startRole(ctx, "host-agent", "normal");
    const launch = launches[0]!;
    expect(launch.role).toBe("host-agent");
    expect(launch.entrypoint).toBeUndefined();
    expect(launch.args).toBeUndefined();
    expect(launch.options).toContain(`container:${"e".repeat(64)}`);
    expect(launch.options[launch.options.indexOf("--pid") + 1]).toBe("host");
    expect(launch.options[launch.options.indexOf("--cgroupns") + 1]).toBe(
      "private",
    );
    expect(launch.options).toContain("/dev/nbd0:/dev/nbd0:rw");
    expect(launch.options).toContain("/dev/nbd1:/dev/nbd1:rw");
    expect(launch.options).toContain("/dev/kvm:/dev/kvm:rw");
    expect(launch.options).toContain("/dev/net/tun:/dev/net/tun:rw");
    const envPath = launch.options[launch.options.indexOf("--env-file") + 1]!;
    const actualEnv = Object.fromEntries(
      (await readFile(envPath, "utf8"))
        .trim()
        .split("\n")
        .map((line) => {
          const i = line.indexOf("=");
          return [line.slice(0, i), line.slice(i + 1)];
        }),
    );
    const declared = JSON.parse(
      await readFile(
        ctx.supervisor.configurations["host-agent"].normal.path,
        "utf8",
      ),
    );
    expect(actualEnv).toEqual(declared.env);
    expect(actualEnv.ENGRAM_GRPC_LISTEN_ADDR).toBe("0.0.0.0:9101");
    expect(actualEnv.ENGRAM_GRPC_ADVERTISE_ADDR).toBe(
      "https://host.fixture.example:9101",
    );
    expect(actualEnv.ENGRAM_FIRECRACKER_BIN).toBe("/usr/local/bin/firecracker");
    const coordinatorConfiguration = JSON.parse(
      await readFile(
        ctx.supervisor.configurations.coordinator.normal.path,
        "utf8",
      ),
    );
    const orchestratorConfiguration = JSON.parse(
      await readFile(
        ctx.supervisor.configurations.orchestrator.normal.path,
        "utf8",
      ),
    );
    expect(
      coordinatorConfiguration.env.ENGRAM_AUTH_TOKENS.split(","),
    ).toContain(orchestratorConfiguration.env.CONTROL_PLANE_BEARER);
    expect(
      coordinatorConfiguration.env.ENGRAM_AUTH_TOKENS.split(","),
    ).toContain(declared.env.ENGRAM_COORDINATOR_TOKEN);
    const workerConfiguration = JSON.parse(
      await readFile(ctx.supervisor.configurations.worker.normal.path, "utf8"),
    );
    expect(Object.keys(workerConfiguration.env).sort()).toEqual([
      "MCP_WORKER_ENABLED",
      "NODE_ENV",
    ]);
    expect(Object.keys(workerConfiguration.files).sort()).toEqual([
      "config.json",
      "tls/ca.crt",
      "tls/tls.crt",
      "tls/tls.key",
    ]);
    const coordinatorDsn = await readFile(
        ctx.observer.coordinatorDsnFile,
        "utf8",
      ),
      orchestratorDsn = await readFile(
        ctx.observer.orchestratorDsnFile,
        "utf8",
      );
    expect(ctx.observer.coordinatorDsnFile).not.toBe(
      ctx.observer.orchestratorDsnFile,
    );
    expect(new URL(coordinatorDsn).hostname).toBe("postgres.fixture.example");
    expect(new URL(orchestratorDsn).pathname).toBe(
      `/mcp_${identity.runId.replaceAll("-", "_")}`,
    );
    expect(coordinatorDsn).toBe(orchestratorDsn);
    engine.guardIds.set("worker", "f".repeat(64));
    await stack.startRole(ctx, "worker", "normal");
    const workerLaunch = launches.find((v) => v.role === "worker")!;
    expect(workerLaunch.options).not.toContain("--cap-add");
    await stack.dependencies();
    for (const role of ["dns", "denied-dns"]) {
      const dnsLaunch = launches.find((v) => v.role === role)!;
      expect(dnsLaunch.entrypoint).toBe("/coredns");
      expect(dnsLaunch.options).toContain("--read-only");
      expect(dnsLaunch.options[dnsLaunch.options.indexOf("--user") + 1]).toBe(
        "1000:1000",
      );
      expect(
        dnsLaunch.options.flatMap((v, i) =>
          v === "--cap-add" ? [dnsLaunch.options[i + 1]] : [],
        ),
      ).toEqual(["NET_BIND_SERVICE"]);
    }
    for (const launch of launches.filter(
      (v) => !["dns", "denied-dns"].includes(v.role),
    ))
      expect(launch.options).not.toContain("NET_BIND_SERVICE");
  } finally {
    await rm(root, { recursive: true });
  }
});

test("archive provenance rejects a swapped config, manifest, or multi-platform index", async () => {
  const { verifyArchiveIdentity } = await import("./assets.ts");
  const config = Buffer.from('{"architecture":"arm64","os":"linux"}');
  const id = `sha256:${hash(config)}`;
  const manifest = Buffer.from(
    JSON.stringify({ schemaVersion: 2, config: { digest: id }, layers: [] }),
  );
  const image = {
    archive: { path: "/owned/image.tar", sha256: "a".repeat(64) },
    id,
    manifestDigest: `sha256:${hash(manifest)}`,
  };
  expect(() => verifyArchiveIdentity(image, manifest, config)).not.toThrow();
  expect(() =>
    verifyArchiveIdentity(image, manifest, Buffer.from("different")),
  ).toThrow("archive_config_identity");
  expect(() =>
    verifyArchiveIdentity(
      { ...image, manifestDigest: image.id },
      manifest,
      config,
    ),
  ).toThrow("archive_image_identity");
  const index = Buffer.from(
    JSON.stringify({ schemaVersion: 2, manifests: [], config: { digest: id } }),
  );
  expect(() =>
    verifyArchiveIdentity(
      { ...image, manifestDigest: `sha256:${hash(index)}` },
      index,
      config,
    ),
  ).toThrow("archive_image_identity");
});

test("resolver uses only a readonly named file projection and rejects source, subpath and byte substitution", async () => {
  const root = await mkdtemp(
    join(process.cwd(), "var/mcp-platform-composition/resolver-"),
  );
  const runId = "mcp-123456789012345678901234",
    image = `sha256:${"b".repeat(64)}`,
    id = "a".repeat(64),
    network = "e".repeat(64),
    volume = `${runId}-resolver`;
  const mount = {
    Type: "volume",
    Source: volume,
    Target: "/etc/resolv.conf",
    ReadOnly: true,
    VolumeOptions: { Subpath: "resolv.conf", NoCopy: true },
  };
  const observed = {
    Type: "volume",
    Name: volume,
    Source: `/engine/volumes/${volume}/_data`,
    Destination: "/etc/resolv.conf",
    RW: false,
  };
  const state = {
    Id: id,
    Image: image,
    Config: {
      Labels: { "engrams.mcp.run": runId },
      User: "1000:1000",
      Env: ["PATH=/usr/bin:/bin"],
      Entrypoint: ["/bin/sleep"],
      Cmd: ["infinity"],
    },
    HostConfig: {
      RestartPolicy: { Name: "no" },
      Privileged: false,
      ReadonlyRootfs: true,
      PidsLimit: 128,
      Memory: 805306368,
      NanoCpus: 2000000000,
      NetworkMode: "none",
      PidMode: "",
      IpcMode: "private",
      CapDrop: ["ALL"],
      CapAdd: [],
      SecurityOpt: ["no-new-privileges"],
      Binds: [],
      VolumesFrom: [],
      Devices: [],
      DeviceRequests: [],
      Mounts: [mount],
    },
    Mounts: [observed],
  };
  const calls: string[][] = [];
  let foreignVolume = false,
    wrongBytes = false;
  class ResolverEngine extends Engine {
    override async project(role: string): Promise<string> {
      return this.volume(role);
    }
    override async call(args: string[]): Promise<Buffer> {
      calls.push(args);
      if (
        (args[0] === "network" && args[1] === "ls") ||
        (args[0] === "volume" && args[1] === "ls")
      )
        return Buffer.from("");
      if (args[0] === "network" && args[1] === "create")
        return Buffer.from(network);
      if (args[0] === "network" && args[1] === "inspect")
        return Buffer.from(
          JSON.stringify([
            {
              Id: network,
              Internal: true,
              Driver: "bridge",
              Labels: { "engrams.mcp.run": runId },
              IPAM: {
                Config: [{ Subnet: "10.42.3.0/24", Gateway: "10.42.3.1" }],
              },
            },
          ]),
        );
      if (args[0] === "volume" && args[1] === "create")
        return Buffer.from(volume);
      if (args[0] === "volume" && args[1] === "inspect")
        return Buffer.from(
          JSON.stringify([
            {
              Name: volume,
              Mountpoint: `/engine/volumes/${volume}/_data`,
              Driver: "local",
              Scope: "local",
              Labels: { "engrams.mcp.run": foreignVolume ? "foreign" : runId },
            },
          ]),
        );
      if (args[0] === "create" || args[0] === "start") return Buffer.from(id);
      if (args[0] === "container" && args[1] === "inspect")
        return Buffer.from(JSON.stringify([state]));
      if (args[0] === "image")
        return Buffer.from(
          JSON.stringify([
            {
              Id: image,
              Config: {
                Env: ["PATH=/usr/bin:/bin"],
                Entrypoint: ["/bin/sleep"],
                Cmd: ["infinity"],
              },
            },
          ]),
        );
      if (args[0] === "cp") {
        expect(args[1]).toBe(`${id}:/etc/resolv.conf`);
        await writeFile(
          args[2]!,
          wrongBytes
            ? "nameserver 127.0.0.11\n"
            : await readFile(join(root, "resolv.conf")),
        );
        return Buffer.from("");
      }
      throw new Error("unexpected resolver operation");
    }
  }
  try {
    const engine = new ResolverEngine(
      { subnet: "10.42.3.0/24", images: { guard: { id: image } } } as Inputs,
      root,
      runId,
    );
    await engine.network();
    await engine.create(
      "probe",
      image,
      ["--network", "none", "--user", "1000:1000", "--read-only"],
      ["infinity"],
      "/bin/sleep",
    );
    expect(calls.find((args) => args[0] === "create")).toContain(
      `type=volume,src=${volume},dst=/etc/resolv.conf,volume-subpath=resolv.conf,volume-nocopy,readonly`,
    );
    mount.VolumeOptions.Subpath = "foreign";
    await expect(engine.audit(id)).rejects.toThrow(
      "container_mount_specification",
    );
    mount.VolumeOptions.Subpath = "resolv.conf";
    mount.ReadOnly = false;
    await expect(engine.audit(id)).rejects.toThrow(
      "container_mount_specification",
    );
    mount.ReadOnly = true;
    const originalSource = observed.Source;
    observed.Source = "/foreign/volume";
    await expect(engine.audit(id)).rejects.toThrow("container_mount");
    observed.Source = originalSource;
    observed.Name = "foreign";
    await expect(engine.audit(id)).rejects.toThrow("container_mount");
    observed.Name = volume;
    foreignVolume = true;
    await expect(engine.audit(id)).rejects.toThrow("volume_identity");
    foreignVolume = false;
    wrongBytes = true;
    await expect(engine.verifyResolver(id)).rejects.toThrow("resolver_changed");
  } finally {
    await rm(root, { recursive: true });
  }
});

test("cleanup recovers only a planned owned container after a lost create response", async () => {
  const root = await mkdtemp(
    join(process.cwd(), "var/mcp-platform-composition/lost-create-"),
  );
  const runId = "mcp-123456789012345678901234",
    id = "a".repeat(64);
  let removed = false;
  const calls: string[][] = [];
  class LostResponseEngine extends Engine {
    override async call(args: string[]): Promise<Buffer> {
      calls.push(args);
      if (args[0] === "create") throw new Error("lost create response");
      if (args[0] === "ps") return Buffer.from(removed ? "" : id);
      if (args[0] === "container" && args[1] === "inspect")
        return Buffer.from(
          JSON.stringify([
            {
              Id: id,
              Name: `/${runId}-probe`,
              Config: { Labels: { "engrams.mcp.run": runId } },
              State: { Running: false, Pid: 0 },
            },
          ]),
        );
      if (args[0] === "logs") return Buffer.from("controlled retained log");
      if (args[0] === "rm") {
        removed = true;
        return Buffer.from(id);
      }
      throw new Error("unexpected cleanup operation");
    }
  }
  try {
    const engine = new LostResponseEngine({} as Inputs, root, runId);
    await expect(
      engine.create("probe", "sha256:" + "b".repeat(64), ["--network", "none"]),
    ).rejects.toThrow("lost create response");
    await engine.cleanup();
    expect(calls.find((args) => args[0] === "rm")).toEqual(["rm", id]);
    expect(
      JSON.parse(await readFile(join(root, "cleanup.json"), "utf8")).success,
    ).toBe(true);
  } finally {
    await rm(root, { recursive: true });
  }
});

async function handoffFixture(root: string) {
  const { ContextSchema } = await import(
    "../../../../orchestrator/scripts/mcp-stack/context.ts"
  );
  const { put } = await import("./owned.ts");
  const pin = await put(join(root, "artifact"), "a".repeat(40));
  const endpoint = (name: string) => ({
    origin: `https://${name}.fixture.example`,
    ca: pin,
    serverFingerprint: Array(32).fill("AA").join(":"),
  });
  const fixture = async (name: "modern" | "legacy") => {
    const runRoot = join(root, name);
    await mkdir(runRoot, { mode: 0o700 });
    const configuration = await put(join(runRoot, "configuration"), "fixture");
    return {
      ...endpoint(`observer.${name}`),
      configuration,
      runRoot,
      tokenFile: configuration.path,
      resourceKeyFile: configuration.path,
      resourceOrigin: `https://resource.${name}.fixture.example`,
      issuerOrigin: `https://issuer.${name}.fixture.example`,
      authorizationOrigin: `https://authorization.${name}.fixture.example`,
      tokenOrigin: `https://token.${name}.fixture.example`,
      redirectUri: "https://app.fixture.example/integrations/mcp/callback",
      protocol: name === "modern" ? "2026-07-28" : "2025-06-18",
    };
  };
  const human = (name: string) => ({
    email: `${name}@fixture.example`,
    passwordFile: pin.path,
    cookieFile: join(root, `${name}.cookie`),
  });
  return ContextSchema.parse({
    version: 1,
    runId: "mcp-controlled-handoff",
    runRoot: root,
    backend: "vz",
    sourceCommit: "b".repeat(40),
    hostId: "00000000-0000-4000-8000-000000000001",
    imageId: "controlled-image",
    imageDigest: "sha256:" + "c".repeat(64),
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
      ].map((name) => [name, pin]),
    ),
    endpoints: {
      app: endpoint("app"),
      coordinator: endpoint("coordinator"),
      workerControl: endpoint("worker"),
      operator: {
        ...endpoint("operator"),
        certificate: pin,
        privateKeyFile: pin.path,
      },
    },
    protocolFixtures: {
      modern: await fixture("modern"),
      legacy: await fixture("legacy"),
    },
    publication: {
      candidateIdentity: "sha256:" + "d".repeat(64),
      expectedGeneration: null,
      deploymentIdentity: "sha256:" + "d".repeat(64),
      networkIdentity: "sha256:" + "d".repeat(64),
      trustIdentity: "sha256:" + "d".repeat(64),
      zone: "fixture-zone",
    },
    identities: {
      admin: human("admin"),
      owner: human("owner"),
      other: human("other"),
      unattendedKeyFile: join(root, "unattended"),
      automationKeyFile: join(root, "automation"),
      routerSentinelFile: pin.path,
    },
    supervisor: {
      socket: join(root, "s"),
      tokenFile: pin.path,
      configurations: {
        orchestrator: { normal: pin },
        coordinator: { normal: pin },
        worker: { normal: pin },
        "host-agent": { normal: pin },
      },
      operatorCandidates: {
        normal: { file: pin, identity: "sha256:" + "d".repeat(64) },
      },
    },
    observer: {
      coordinatorDsnFile: pin.path,
      orchestratorDsnFile: pin.path,
      hostManagedRoot: join(root, "managed"),
      hostLogFile: pin.path,
    },
    guest: {
      stateDirectory: "/tmp/mcp-stack-fixture",
      cliPath: "/opt/engram/dyn/0/engram-mcp-model-fixture",
    },
    deadlines: { requestMs: 1000, stepMs: 3000, totalMs: 10000 },
    evidenceFile: join(root, "evidence"),
  });
}

test("runner handoff validates the complete context with a real private supervisor socket", async () => {
  const { startContextSupervisor } = await import("./main.ts");
  const { loadContext } = await import(
    "../../../../orchestrator/scripts/mcp-stack/context.ts"
  );
  const { lstat } = await import("node:fs/promises");
  const root = await mkdtemp(
    join(process.cwd(), "var/mcp-platform-composition/h-"),
  );
  let supervisor:
    | Awaited<ReturnType<typeof startContextSupervisor>>
    | undefined;
  try {
    const ctx = await handoffFixture(root);
    await mkdir(ctx.observer.hostManagedRoot, { mode: 0o700 });
    const unexpected = async (): Promise<never> => {
      throw new Error("handoff does not need lifecycle access");
    };
    supervisor = await startContextSupervisor(ctx, {
      status: unexpected,
      restart: unexpected,
      stop: unexpected,
    });
    expect((await lstat(ctx.supervisor.socket)).isSocket()).toBe(true);
    expect((await lstat(ctx.supervisor.socket)).mode & 0o077).toBe(0);
    expect(await loadContext(join(root, "context.json"))).toEqual(ctx);
    await supervisor.close();
    supervisor = undefined;
    await expect(lstat(ctx.supervisor.socket)).rejects.toThrow();
  } finally {
    if (supervisor) await supervisor.close();
    await rm(root, { recursive: true });
  }
});

test("failed complete context validation drains and removes the real handoff supervisor", async () => {
  const { startContextSupervisor } = await import("./main.ts");
  const { lstat } = await import("node:fs/promises");
  const root = await mkdtemp(
    join(process.cwd(), "var/mcp-platform-composition/h-failed-"),
  );
  try {
    const ctx = await handoffFixture(root);
    const unexpected = async (): Promise<never> => {
      throw new Error("handoff does not need lifecycle access");
    };
    await expect(
      startContextSupervisor(ctx, {
        status: unexpected,
        restart: unexpected,
        stop: unexpected,
      }),
    ).rejects.toThrow("owned_path");
    await expect(lstat(ctx.supervisor.socket)).rejects.toThrow();
    expect((await lstat(join(root, "context.json"))).isFile()).toBe(true);
  } finally {
    await rm(root, { recursive: true });
  }
});

test("only the coordinator guard installs the exact native-host route on the internal bridge", async () => {
  const calls: string[][] = [];
  const inputs = {
    backend: "firecracker",
    subnet: "10.42.3.0/24",
    nativeHostIp: "10.42.4.1",
    registryPort: 32000,
    images: { guard: { id: "sha256:" + "a".repeat(64) } },
  } as Inputs;
  const ids = {
    orchestrator: "a".repeat(64),
    coordinator: "b".repeat(64),
    worker: "c".repeat(64),
  };
  class GuardPlan extends Engine {
    override async project(): Promise<string> {
      return "controlled-probe-trust";
    }
    override networkOptions(): string[] {
      return ["--network", "controlled-internal"];
    }
    override async create(role: string): Promise<string> {
      return ids[role.slice(0, -6) as keyof typeof ids];
    }
    override async inspect() {
      return {
        HostConfig: {
          PortBindings: {
            "32000/tcp": [{ HostIp: "127.0.0.1", HostPort: "32000" }],
            "9101/tcp": [{ HostIp: "10.42.4.1", HostPort: "9101" }],
          },
        },
      };
    }
    override async call(args: string[]): Promise<Buffer> {
      calls.push(args);
      return Buffer.from("");
    }
    override async verifyNetwork() {
      return {
        generation: "controlled",
        enforcementSha256: "e".repeat(64),
        fixtureOnlyEgress: true as const,
        externalDnsBlocked: true as const,
        roles: ["orchestrator", "coordinator", "worker"] as (
          | "orchestrator"
          | "coordinator"
          | "worker"
        )[],
      };
    }
  }
  const engine = new GuardPlan(inputs, "/owned/run", "mcp-controlled-route");
  await engine.guards({ path: "/owned/ca", sha256: "f".repeat(64) });
  expect(calls.filter((args) => args.includes("/usr/sbin/ip"))).toEqual([
    [
      "exec",
      ids.coordinator,
      "/usr/sbin/ip",
      "-4",
      "route",
      "replace",
      "10.42.4.1/32",
      "via",
      "10.42.3.1",
      "dev",
      "eth0",
    ],
  ]);
});

test("native-host route qualification rejects default, missing, duplicate and substituted routes", async () => {
  const { verifyNativeRouteState } = await import("./engine.ts");
  const inputs = { subnet: "10.42.3.0/24", nativeHostIp: "10.42.4.1" };
  const route = { dst: "10.42.4.1", gateway: "10.42.3.1", dev: "eth0" };
  const selected = { ...route, prefsrc: "10.42.3.21" };
  expect(() =>
    verifyNativeRouteState([route], [selected], inputs),
  ).not.toThrow();
  for (const routes of [
    [],
    [route, route],
    [route, { dst: "default", gateway: "10.42.3.1" }],
    [{ ...route, gateway: "10.42.3.40" }],
    [{ ...route, dev: "foreign0" }],
  ])
    expect(() => verifyNativeRouteState(routes, [selected], inputs)).toThrow();
  for (const current of [
    { ...selected, gateway: "10.42.3.40" },
    { ...selected, prefsrc: "10.42.3.22" },
    { ...selected, dev: "foreign0" },
  ])
    expect(() => verifyNativeRouteState([route], [current], inputs)).toThrow(
      "native_route_selected",
    );
});

test("native-host TLS qualification pins the actual first leaf and removes its private probe on failure", async () => {
  const { generateIdentity } = await import("./identity.ts");
  const { verifyNativeHostTls } = await import("./engine.ts");
  const root = await mkdtemp(
    join(process.cwd(), "var/mcp-platform-composition/host-tls-"),
  );
  const openssl = await realpath(
    process.env.MCP_PLATFORM_TEST_OPENSSL ?? Bun.which("openssl") ?? "openssl",
  );
  const inputs = {
    subnet: "10.42.3.0/24",
    nativeHostIp: "10.42.4.1",
    binaries: {
      openssl: { path: openssl, sha256: hash(await readFile(openssl)) },
    },
    images: { guard: { id: "sha256:" + "a".repeat(64) } },
  } as Inputs;
  try {
    const identity = await generateIdentity(inputs, root),
      id = "a".repeat(64),
      calls: string[][] = [];
    const host = identity.leaves.host,
      wrong = identity.leaves.worker;
    const output = async (certificate: string) =>
      Buffer.from(
        `New, TLSv1.3, Cipher is TLS_AES_256_GCM_SHA384\n${await readFile(certificate, "utf8")}\nVerify return code: 0 (ok)\n`,
      );
    let bytes = await output(host.certificate.path),
      running = false,
      removed = true;
    class TlsProbe extends Engine {
      override async project(
        _role: string,
        files: Record<string, import("./owned.ts").Pin>,
      ): Promise<string> {
        expect(Object.keys(files).sort()).toEqual([
          "ca.pem",
          "client.crt",
          "client.key",
        ]);
        return "controlled-client-tls";
      }
      override async verifyNetwork() {
        return {
          generation: "controlled",
          enforcementSha256: "e".repeat(64),
          fixtureOnlyEgress: true as const,
          externalDnsBlocked: true as const,
          roles: ["orchestrator", "coordinator", "worker"] as (
            | "orchestrator"
            | "coordinator"
            | "worker"
          )[],
        };
      }
      override async audit(id: string) {
        return { Id: id };
      }
      override async call(args: string[]): Promise<Buffer> {
        calls.push(args);
        if (args[0] === "create") {
          removed = false;
          return Buffer.from(id);
        }
        if (args[0] === "start") {
          running = true;
          return Buffer.from(id);
        }
        if (args[0] === "exec" && args[2] === "/usr/bin/openssl") return bytes;
        if (args[0] === "container" && args[1] === "inspect")
          return Buffer.from(
            JSON.stringify([
              {
                Id: id,
                Config: { Labels: { "engrams.mcp.run": identity.runId } },
                State: { Running: running, Pid: running ? 123 : 0 },
              },
            ]),
          );
        if (args[0] === "stop") {
          running = false;
          return Buffer.from(id);
        }
        if (args[0] === "rm") {
          removed = true;
          return Buffer.from(id);
        }
        if (args[0] === "ps") return Buffer.from(removed ? "" : id);
        throw new Error("unexpected fixed TLS probe command");
      }
    }
    const engine = new TlsProbe(inputs, root, identity.runId);
    engine.guardIds.set("coordinator", "b".repeat(64));
    const tls = {
      ca: identity.ca,
      clientCertificate: identity.leaves.coordinator.certificate,
      clientKey: identity.leaves.coordinator.key,
      serverCertificate: host.certificate,
    };
    await engine.qualifyNativeHost(tls);
    await engine.qualifyNativeHost(tls);
    expect(removed).toBe(true);
    const created = calls.find((args) => args[0] === "create")!;
    expect(created).toContain(`container:${"b".repeat(64)}`);
    expect(created).toContain(
      "type=volume,src=controlled-client-tls,dst=/etc/mcp-host-probe,readonly",
    );
    expect(created).not.toContain("--env-file");
    expect(created).not.toContain("--cap-add");
    expect(created).not.toContain("--device");
    const invoked = calls.find(
      (args) => args[0] === "exec" && args[2] === "/usr/bin/openssl",
    )!;
    for (const flag of [
      "-tls1_3",
      "-verify_return_error",
      "-verify_hostname",
      "-servername",
      "-cert",
      "-key",
    ])
      expect(invoked).toContain(flag);
    bytes = await output(wrong.certificate.path);
    await expect(engine.qualifyNativeHost(tls)).rejects.toThrow(
      "native_host_tls_leaf",
    );
    expect(removed).toBe(true);
    expect(running).toBe(false);
    const correct = await output(host.certificate.path);
    expect(() =>
      verifyNativeHostTls(Buffer.concat([bytes, correct]), host.fingerprint),
    ).toThrow("native_host_tls_leaf");
    expect(() =>
      verifyNativeHostTls(
        Buffer.from(correct.toString().replace("TLSv1.3", "TLSv1.2")),
        host.fingerprint,
      ),
    ).toThrow("native_host_tls_verification");
    expect(() =>
      verifyNativeHostTls(
        Buffer.from(correct.toString().replace("0 (ok)", "20 (bad)")),
        host.fingerprint,
      ),
    ).toThrow("native_host_tls_verification");
  } finally {
    await rm(root, { recursive: true });
  }
});

test("FC sysctls belong only to its C guard and actual firewall order stays production exact", async () => {
  const { fcGuardSysctls, fcHostFirewall, verifyFcHostFirewall } = await import(
    "./engine.ts"
  );
  expect(fcGuardSysctls("firecracker", "coordinator-guard")).toEqual({
    "net.ipv4.ip_forward": "1",
    "net.ipv4.conf.all.route_localnet": "1",
  });
  for (const backend of ["firecracker", "vz"] as const)
    for (const role of [
      "orchestrator-guard",
      "worker-guard",
      "host-agent",
      "worker",
      ...(backend === "vz" ? ["coordinator-guard"] : []),
    ])
      expect(fcGuardSysctls(backend, role)).toEqual({});
  const expected = fcHostFirewall(26000);
  expect(expected.INPUT[0]).toContain("--dport 26003");
  expect(expected.FORWARD.at(-1)).toContain("engram-default-deny");
  expect(expected.PREROUTING[0]).toContain("-i vh-engr-+");
  expect(expected.PREROUTING[2]).toContain("--dport 443");
  const actual = Object.fromEntries(
    Object.entries(expected).map(([chain, lines]) => [
      chain,
      [
        `-P ${chain} ACCEPT`,
        ...lines.map((line) =>
          line
            .replace("-p tcp", "-p tcp -m tcp")
            .replace("-p udp", "-p udp -m udp")
            .replace("--to-port ", "--to-ports ")
            .replace(/--comment (\S+)/, '--comment "$1"'),
        ),
      ].join("\n") + "\n",
    ]),
  );
  verifyFcHostFirewall(actual, 26000);
  for (const current of [
    {
      ...actual,
      INPUT: actual.INPUT!.replace("--dport 26003", "--dport 26004"),
    },
    { ...actual, INPUT: actual.INPUT!.replace(/-A INPUT[^\n]+\n/, "") },
    {
      ...actual,
      FORWARD: actual.FORWARD!.replace(
        "-P FORWARD ACCEPT\n",
        "-P FORWARD ACCEPT\n-A FORWARD -j ACCEPT\n",
      ),
    },
    {
      ...actual,
      PREROUTING: actual.PREROUTING!.replace("-i vh-engr-+", "-i tap-engr-+"),
    },
    { ...actual, POSTROUTING: "-P POSTROUTING ACCEPT\n" },
    {
      ...actual,
      INPUT: actual
        .INPUT!.split("\n")
        .map((line, i, all) => (i === 1 ? all[5] : i === 5 ? all[1] : line))
        .join("\n"),
    },
  ])
    expect(() => verifyFcHostFirewall(current, 26000)).toThrow();
});

test("FC qualification reads both actual namespace sysctls and production chains", async () => {
  const { fcHostFirewall } = await import("./engine.ts");
  const root = await mkdtemp(
    join(process.cwd(), "var/mcp-platform-composition/fc-net-"),
  );
  const calls: string[][] = [];
  let badNamespace = false,
    badSysctl = false;
  class FcProbe extends Engine {
    override async call(args: string[]) {
      calls.push(args);
      if (args[2] === "/usr/bin/readlink")
        return Buffer.from(
          badNamespace && args[1] === "host" ? "net:[456]\n" : "net:[123]\n",
        );
      if (args[2] === "/bin/cat") return Buffer.from(badSysctl ? "0\n" : "1\n");
      if (args[2] === "/usr/sbin/iptables") {
        const chain = args[6]!;
        return Buffer.from(
          [`-P ${chain} ACCEPT`, ...fcHostFirewall(26000)[chain]!].join("\n") +
            "\n",
        );
      }
      throw new Error("unexpected FC qualification command");
    }
  }
  try {
    const engine = new FcProbe(
      { backend: "firecracker", registryPort: 26000 } as Inputs,
      root,
      "controlled",
    );
    engine.guardIds.set("coordinator", "guard");
    await engine.qualifyFcHost("host");
    expect(calls.filter((args) => args[2] === "/bin/cat")).toHaveLength(4);
    expect(
      calls.filter((args) => args[2] === "/usr/sbin/iptables"),
    ).toHaveLength(4);
    await engine.qualifyFcHost("host");
    badNamespace = true;
    await expect(engine.qualifyFcHost("host")).rejects.toThrow(
      "fc_host_namespace",
    );
    badNamespace = false;
    badSysctl = true;
    await expect(engine.qualifyFcHost("host")).rejects.toThrow(
      "fc_host_sysctl",
    );
  } finally {
    await rm(root, { recursive: true });
  }
});

test("VZ link uses private nonoverlapping IPAM and derives its interface from the exact endpoint", async () => {
  const { verifyNativeSubnet, nativeInterface, verifyNativeRouteState } =
    await import("./engine.ts");
  expect(() =>
    verifyNativeSubnet("172.25.0.0/16", "172.25.0.1", [
      "10.42.3.0/24",
      "192.168.1.5",
    ]),
  ).not.toThrow();
  for (const [subnet, gateway, forbidden] of [
    ["8.8.8.0/24", "8.8.8.1", []],
    ["172.25.0.0/16", "172.26.0.1", []],
    ["172.25.0.0/16", "172.25.0.1", ["172.25.3.0/24"]],
    ["172.25.0.0/16", "172.25.0.1", ["172.25.3.5"]],
    ["172.25.3.0/16", "172.25.3.1", []],
  ] as [string, string, string[]][])
    expect(() => verifyNativeSubnet(subnet, gateway, forbidden)).toThrow();
  const value = [
    {
      ifname: "owned7",
      address: "02:42:ac:19:00:02",
      addr_info: [{ family: "inet", local: "172.25.0.2" }],
    },
  ];
  expect(nativeInterface(value, "172.25.0.2", "02:42:ac:19:00:02")).toBe(
    "owned7",
  );
  expect(() =>
    nativeInterface(
      [{ ...value[0], addr_info: [{ family: "inet6", local: "172.25.0.2" }] }],
      "172.25.0.2",
      "02:42:ac:19:00:02",
    ),
  ).toThrow("native_link_interface");
  expect(() =>
    nativeInterface(value, "172.25.0.3", "02:42:ac:19:00:02"),
  ).toThrow();
  expect(() =>
    nativeInterface([...value, ...value], "172.25.0.2", "02:42:ac:19:00:02"),
  ).toThrow();
  expect(() =>
    nativeInterface(value, "172.25.0.2", "02:42:ac:19:00:03"),
  ).toThrow();
  const inputs = { subnet: "10.42.3.0/24", nativeHostIp: "192.168.1.5" };
  const link = { gateway: "172.25.0.1", dev: "owned7", source: "172.25.0.2" };
  const route = {
    dst: inputs.nativeHostIp,
    gateway: link.gateway,
    dev: link.dev,
  };
  expect(() =>
    verifyNativeRouteState(
      [route],
      [{ ...route, prefsrc: link.source }],
      inputs,
      link,
    ),
  ).not.toThrow();
  expect(() =>
    verifyNativeRouteState(
      [route, { dst: "default" }],
      [{ ...route, prefsrc: link.source }],
      inputs,
      link,
    ),
  ).toThrow();
});

test("cleanup selects only authenticated fixture or managed-principal sessions on the exact host", async () => {
  const { ownedCleanupSessions } = await import("./main.ts");
  const id = "11111111-1111-4111-8111-111111111111";
  const session = { id, status: "active", hostId: "owned-host" };
  expect(
    ownedCleanupSessions(
      [{ session, ownerEmail: "owner@fixture.example" }],
      "owned-host",
      new Map(),
      new Set(),
    ),
  ).toEqual([id]);
  expect(
    ownedCleanupSessions(
      [{ session }],
      "owned-host",
      new Map([[id, "owned-actor"]]),
      new Set(["owned-actor"]),
    ),
  ).toEqual([id]);
  expect(() =>
    ownedCleanupSessions(
      [{ session, ownerEmail: "foreign@example.test" }],
      "owned-host",
      new Map(),
      new Set(),
    ),
  ).toThrow("cleanup_foreign_session");
  expect(() =>
    ownedCleanupSessions(
      [
        {
          session: { ...session, hostId: "foreign-host" },
          ownerEmail: "owner@fixture.example",
        },
      ],
      "owned-host",
      new Map(),
      new Set(),
    ),
  ).toThrow();
  expect(() =>
    ownedCleanupSessions(
      [{ session }, { session }],
      "owned-host",
      new Map([[id, "owned-actor"]]),
      new Set(["owned-actor"]),
    ),
  ).toThrow("cleanup_session_identity");
  expect(
    ownedCleanupSessions(
      [{ session: { ...session, status: "completed" } }],
      "owned-host",
      new Map(),
      new Set(),
    ),
  ).toEqual([]);
});

test("VZ guard acquires and revalidates its exact link with complete address metadata", async () => {
  const calls: string[][] = [];
  const id = "b".repeat(64),
    network = "d".repeat(64),
    runId = "mcp-controlled-vz";
  let connected = false,
    removed = false;
  const endpoint = {
    IPv4Address: "172.25.0.2/16",
    MacAddress: "02:42:ac:19:00:02",
  };
  const interfaces = [
    {
      ifname: "owned7",
      address: endpoint.MacAddress,
      addr_info: [
        { family: "inet6", local: "fe80::42:acff:fe19:2" },
        { family: "inet", local: "172.25.0.2" },
      ],
    },
  ];
  class VzPlan extends Engine {
    override async project() {
      return "controlled-trust";
    }
    override networkOptions() {
      return ["--network", "controlled-internal"];
    }
    override async create(role: string) {
      return role === "coordinator-guard"
        ? id
        : role === "worker-guard"
          ? "c".repeat(64)
          : "a".repeat(64);
    }
    override async inspect() {
      return {
        HostConfig: {
          PortBindings: {
            "32000/tcp": [{ HostIp: "127.0.0.1", HostPort: "32000" }],
          },
        },
      };
    }
    override async verifyNetwork() {
      return {
        generation: "controlled",
        enforcementSha256: "e".repeat(64),
        fixtureOnlyEgress: true as const,
        externalDnsBlocked: true as const,
        roles: ["orchestrator", "coordinator", "worker"] as (
          | "orchestrator"
          | "coordinator"
          | "worker"
        )[],
      };
    }
    override async call(args: string[]) {
      calls.push(args);
      if (args[0] === "network" && args[1] === "create")
        return Buffer.from(network);
      if (args[0] === "network" && args[1] === "connect") {
        connected = true;
        return Buffer.from("");
      }
      if (args[0] === "network" && args[1] === "inspect")
        return Buffer.from(
          JSON.stringify([
            {
              Id: network,
              Name: `${runId}-native-network`,
              Internal: false,
              Driver: "bridge",
              EnableIPv6: false,
              Labels: { "engrams.mcp.run": runId },
              IPAM: {
                Config: [{ Subnet: "172.25.0.0/16", Gateway: "172.25.0.1" }],
              },
              Containers: connected ? { [id]: endpoint } : {},
            },
          ]),
        );
      if (args.includes("address"))
        return Buffer.from(
          JSON.stringify(
            args.includes("-4")
              ? interfaces.map(({ ifname, addr_info }) => ({
                  ifname,
                  addr_info: addr_info.filter((info) => info.family === "inet"),
                }))
              : interfaces,
          ),
        );
      if (
        args.includes("route") &&
        (args.includes("table") || args.includes("get"))
      )
        return Buffer.from(
          JSON.stringify([
            {
              dst: "192.168.254.1",
              gateway: "172.25.0.1",
              dev: "owned7",
              ...(args.includes("get") ? { prefsrc: "172.25.0.2" } : {}),
            },
          ]),
        );
      if (args.at(-1) === "default")
        return Buffer.from(
          JSON.stringify([
            { dst: "default", gateway: "172.25.0.1", dev: "owned7" },
          ]),
        );
      if (args[0] === "network" && args[1] === "rm") {
        removed = true;
        return Buffer.from(network);
      }
      return Buffer.from("");
    }
  }
  const engine = new VzPlan(
    {
      backend: "vz",
      subnet: "10.42.3.0/24",
      nativeHostIp: "192.168.254.1",
      registryPort: 32000,
      images: { guard: { id: "sha256:" + "a".repeat(64) } },
    } as Inputs,
    "/owned/run",
    runId,
  );
  await engine.guards({ path: "/owned/ca", sha256: "f".repeat(64) });
  await engine.verifyNativeRoute();
  const attach = calls.findIndex(
    (args) => args[0] === "network" && args[1] === "connect",
  );
  expect(
    calls.slice(0, attach).filter((args) => args.includes("/usr/sbin/nft")),
  ).toHaveLength(3);
  expect(calls[attach]).toEqual(["network", "connect", network, id]);
  expect(calls).toContainEqual([
    "exec",
    id,
    "/usr/sbin/ip",
    "-4",
    "route",
    "del",
    "default",
    "via",
    "172.25.0.1",
    "dev",
    "owned7",
  ]);
  expect(calls).toContainEqual([
    "exec",
    id,
    "/usr/sbin/ip",
    "-4",
    "route",
    "replace",
    "192.168.254.1/32",
    "via",
    "172.25.0.1",
    "dev",
    "owned7",
  ]);
  expect(removed).toBe(false);
});

test("FC process proof rejects a recycled PID, foreign cgroup, UID or executable", async () => {
  const { verifyFcProcessIdentity } = await import("./fc-runtime.ts");
  const process = {
    pid: 42,
    bootId: "fixed-boot",
    birth: "100",
    uid: 0,
    executable: "/usr/local/bin/firecracker",
    cgroup: "/owned/container",
    sha256: "a".repeat(64),
  };
  expect(() => verifyFcProcessIdentity(process, { ...process })).not.toThrow();
  for (const actual of [
    undefined,
    { ...process, birth: "101" },
    { ...process, bootId: "foreign-boot" },
    { ...process, uid: 1000 },
    { ...process, executable: "/foreign/firecracker" },
    { ...process, cgroup: "/foreign/container" },
    { ...process, sha256: "b".repeat(64) },
  ])
    expect(() => verifyFcProcessIdentity(process, actual)).toThrow(
      "fc_process_identity_changed",
    );
});

test("secret projection reads back private file and directory modes and refuses a public key", async () => {
  const root = await mkdtemp(
    join(process.cwd(), "var/mcp-platform-composition/private-projection-"),
  );
  const { put } = await import("./owned.ts");
  const file = await put(join(root, "client.key"), "controlled-private-key");
  let publicKey = false,
    removed = false;
  class Projection extends Engine {
    override async volume() {
      return "controlled-volume";
    }
    override async create() {
      return "a".repeat(64);
    }
    override async remove() {
      removed = true;
    }
    override async call(args: string[]) {
      if (args.includes("/usr/bin/stat"))
        return Buffer.from(
          args.includes("%u:%g:%a:%F:%h")
            ? `1000:1000:${publicKey ? "644" : "600"}:regular file:1\n`
            : "1000:1000:700:directory\n",
        );
      if (args.includes("/usr/bin/sha256sum"))
        return Buffer.from(file.sha256 + "  /projection/client.key\n");
      return Buffer.from("");
    }
  }
  try {
    const engine = new Projection(
      { images: { guard: { id: "sha256:" + "a".repeat(64) } } } as Inputs,
      root,
      "controlled",
    );
    await expect(
      engine.project("host-tls", { "client.key": file }),
    ).resolves.toBe("controlled-volume");
    expect(removed).toBe(true);
    publicKey = true;
    await expect(
      engine.project("host-tls", { "client.key": file }),
    ).rejects.toThrow("projection_file_mode");
  } finally {
    await rm(root, { recursive: true });
  }
});

for (const partial of [false, true])
  test(`FC evidence write failure stays fatal after a later sample (${partial ? "partial append" : "missing append"})`, async () => {
    const { FcRuntime } = await import("./fc-runtime.ts");
    const { appendFile } = await import("node:fs/promises");
    const root = await mkdtemp(
      join(process.cwd(), "var/mcp-platform-composition/evidence-fault-"),
    );
    const path = join(root, "fc-physical.jsonl");
    const record =
      JSON.stringify({
        sandboxId: "controlled-sandbox",
        bootAttempt: "controlled-boot",
        readSectors: "1",
      }) + "\n";
    let writes = 0;
    const runtime = new FcRuntime(
      {
        bootId: "controlled",
        release: "controlled",
        pidNamespace: "pid:[1]",
        slots: [],
        readSectors: {},
      },
      {} as Inputs,
      root,
      async (file, bytes) => {
        writes++;
        if (writes === 1) {
          if (partial)
            await appendFile(file, bytes.slice(0, 15), { mode: 0o600 });
          throw new Error("controlled evidence write failure");
        }
        await appendFile(file, bytes, { mode: 0o600 });
      },
    );
    try {
      await expect(
        runtime.retainObservation("controlled-sandbox:controlled-boot", record),
      ).rejects.toThrow("controlled evidence write failure");
      // This would be a successful unrelated sample if the failure were lost.
      await expect(runtime.observe()).rejects.toThrow(
        "fc_physical_evidence_write",
      );
      await expect(runtime.requireCoverage()).rejects.toThrow(
        "fc_physical_evidence_write",
      );
      await expect(
        runtime.retainObservation("another-sandbox:another-boot", record),
      ).rejects.toThrow("fc_physical_evidence_write");
      expect(writes).toBe(1);
      if (partial) {
        const retained = await readFile(path, "utf8");
        expect(retained).toBe(record.slice(0, 15));
        expect(() => JSON.parse(retained)).toThrow();
      } else
        await expect(readFile(path)).rejects.toMatchObject({ code: "ENOENT" });
    } finally {
      await rm(root, { recursive: true });
    }
  });
