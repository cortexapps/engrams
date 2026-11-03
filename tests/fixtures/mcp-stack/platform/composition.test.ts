import { test, expect } from "bun:test";
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
import { hash } from "./owned.ts";
import type { Inputs } from "./preflight.ts";

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

test("effective container audit rejects added authority and environment", async () => {
  const id = "a".repeat(64),
    image = "sha256:" + "b".repeat(64),
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
      CapAdd: [],
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
  class Inspector extends Engine {
    override async call(args: string[]): Promise<Buffer> {
      if (args[0] === "create") return Buffer.from(id);
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
      if (args[0] === "start") return Buffer.from(id);
      throw new Error("unexpected fixed test operation");
    }
  }
  const engine = new Inspector({} as Inputs, "/owned/run", runId);
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
              IPAM: { Config: [{ Subnet: "10.42.3.0/24" }] },
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
    const engine = new LaunchEngine(inputs, root, identity.runId);
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
    expect(launch.options).not.toContain("--pid");
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
              IPAM: { Config: [{ Subnet: "10.42.3.0/24" }] },
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
