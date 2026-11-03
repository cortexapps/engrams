import { randomBytes } from "node:crypto";
import { lookup } from "node:dns/promises";
import { lstat, mkdir, readFile, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { join } from "node:path";
import {
  childEnvironment,
  check,
  OwnedProcess,
  hash,
  PlatformFailure,
  put,
  readOwned,
  verifyPin,
  ownedPath,
  type Pin,
} from "./owned.ts";
import {
  address,
  dnsRecords,
  effectiveRules,
  GUARDS,
  rules,
  type GuardRole,
} from "./network.ts";
import {
  type Inputs,
  REPO,
  engineArguments,
  validateEngineConfig,
} from "./preflight.ts";

/** Fixed per-run Linux resources. No application request reaches these commands. */
export class Engine {
  private readonly containers = new Map<string, string>();
  private readonly plannedContainers = new Map<string, string>();
  private readonly volumes = new Map<string, string>();
  private networkId: string | undefined;
  private plannedNetworkName: string | undefined;
  readonly guardIds = new Map<GuardRole, string>();
  private readonly generation = randomBytes(16).toString("hex");
  private baseline: string | undefined;
  private configState:
    | Awaited<ReturnType<typeof validateEngineConfig>>
    | undefined;
  private socketState: { ino: number; dev: number } | undefined;
  private resolver: Pin | undefined;
  private resolverVolume: string | undefined;
  private readonly specifications = new Map<
    string,
    {
      image: string;
      options: string[];
      args: string[];
      entrypoint?: string;
      host: boolean;
    }
  >();
  private signal: AbortSignal | undefined;
  checkCancellation() {
    this.signal?.throwIfAborted();
  }
  async controlled<T>(
    signal: AbortSignal,
    operation: () => Promise<T>,
  ): Promise<T> {
    check(!this.signal, "engine_control_busy");
    this.signal = signal;
    try {
      signal.throwIfAborted();
      return await operation();
    } finally {
      this.signal = undefined;
    }
  }
  private async engineIdentity() {
    this.configState = await validateEngineConfig(
      this.inputs.engineConfig,
      this.configState,
    );
    await ownedPath(this.inputs.engineSocket);
    const socket = await lstat(this.inputs.engineSocket);
    check(socket.isSocket(), "engine_socket");
    if (this.socketState)
      check(
        socket.ino === this.socketState.ino &&
          socket.dev === this.socketState.dev,
        "engine_socket_changed",
      );
    else this.socketState = { ino: socket.ino, dev: socket.dev };
  }
  constructor(
    readonly inputs: Inputs,
    readonly root: string,
    readonly runId: string,
  ) {}
  private label() {
    return `engrams.mcp.run=${this.runId}`;
  }
  private name(role: string) {
    check(/^[a-z0-9-]{1,64}$/.test(role), "resource_role");
    return `${this.runId}-${role}`;
  }
  async call(args: string[], stdin?: string, timeout = 10000): Promise<Buffer> {
    this.signal?.throwIfAborted();
    await this.engineIdentity();
    await verifyPin(this.inputs.binaries.docker);
    const prefix = engineArguments(this.inputs).slice(0, 4);
    try {
      const bytes = await this.runEngine([...prefix, ...args], stdin, timeout);
      return bytes;
    } finally {
      await this.engineIdentity();
    }
  }
  private async runEngine(
    args: string[],
    input: string | undefined,
    timeout: number,
  ): Promise<Buffer> {
    check(
      Number.isInteger(timeout) && timeout > 0 && timeout <= 120000,
      "engine_deadline",
    );
    this.checkCancellation();
    const process = new OwnedProcess(
      this.inputs.binaries.docker.path,
      args,
      REPO,
      childEnvironment(this.root),
      ["pipe", "pipe", "ignore"],
    );
    const { stdin, stdout } = process.child;
    check(stdin && stdout, "engine_stdio");
    const chunks: Buffer[] = [];
    let size = 0,
      failed = false;
    let cancel!: () => void;
    const cancelled = new Promise<void>((resolve) => {
      cancel = resolve;
    });
    const interrupt = () => {
      failed = true;
      cancel();
    };
    const timer = setTimeout(interrupt, timeout);
    const signal = this.signal;
    signal?.addEventListener("abort", interrupt, { once: true });
    stdout.on("data", (bytes: Buffer) => {
      size += bytes.length;
      if (size > 4 * 1024 * 1024) interrupt();
      else chunks.push(bytes);
    });
    stdout.once("error", interrupt);
    stdin.once("error", () => {});
    process.child.once("error", interrupt);
    stdin.end(input);
    try {
      await Promise.race([process.done, cancelled]);
      await process.stop(250);
      check(!failed && process.child.exitCode === 0, "engine_command");
      signal?.throwIfAborted();
      return Buffer.concat(chunks);
    } finally {
      clearTimeout(timer);
      signal?.removeEventListener("abort", interrupt);
      stdin.destroy();
      stdout.destroy();
      process.child.unref();
    }
  }
  async inspect(id: string) {
    const objects = JSON.parse(
      (await this.call(["container", "inspect", id])).toString(),
    );
    check(
      Array.isArray(objects) &&
        objects.length === 1 &&
        objects[0]?.Id === id &&
        objects[0]?.Config?.Labels?.["engrams.mcp.run"] === this.runId,
      "owned_container",
    );
    return objects[0];
  }
  async create(
    role: string,
    image: string,
    options: string[],
    args: string[] = [],
    entrypoint?: string,
  ): Promise<string> {
    check(!this.containers.has(role), "duplicate_role");
    if (this.resolverVolume)
      options = [
        ...options,
        "--mount",
        `type=volume,src=${this.resolverVolume},dst=/etc/resolv.conf,volume-subpath=resolv.conf,volume-nocopy,readonly`,
      ];
    const gated = [
      "orchestrator",
      "coordinator",
      "worker",
      "host-agent",
      "orchestrator-migrate",
    ].includes(role);
    const host = role === "host-agent";
    let selectedArgs = args,
      selectedEntrypoint = entrypoint;
    if (gated) {
      const [imageConfig] = JSON.parse(
        (await this.call(["image", "inspect", image])).toString(),
      );
      const entry = entrypoint
        ? [entrypoint]
        : (imageConfig.Config.Entrypoint ?? []);
      const commandArgs = args.length ? args : (imageConfig.Config.Cmd ?? []);
      check([...entry, ...commandArgs].length > 0, "application_entrypoint");
      selectedEntrypoint = "/bin/sh";
      selectedArgs = [
        "-ec",
        'while [ ! -f /tmp/mcp-start ]; do sleep 0.05; done; if [ -n "${MCP_FIXTURE_HOST_LOG:-}" ]; then exec "$@" >> "$MCP_FIXTURE_HOST_LOG" 2>&1; fi; exec "$@"',
        "mcp-start",
        ...entry,
        ...commandArgs,
      ];
    }
    this.plannedContainers.set(this.name(role), role);
    const id = (
      await this.call([
        "create",
        "--pull=never",
        "--name",
        this.name(role),
        "--label",
        this.label(),
        "--restart=no",
        "--pids-limit",
        host ? "1024" : "128",
        "--memory",
        host ? "8g" : "768m",
        "--cpus",
        host ? "4" : "2",
        "--ulimit",
        "core=0",
        "--security-opt",
        "no-new-privileges",
        "--cap-drop",
        "ALL",
        ...options,
        ...(selectedEntrypoint ? ["--entrypoint", selectedEntrypoint] : []),
        image,
        ...selectedArgs,
      ])
    )
      .toString()
      .trim();
    check(/^[a-f0-9]{64}$/.test(id), "container_id");
    this.containers.set(role, id);
    this.specifications.set(id, {
      image,
      options,
      host,
      args: selectedArgs,
      ...(selectedEntrypoint ? { entrypoint: selectedEntrypoint } : {}),
    });
    await this.audit(id);
    if (this.resolverVolume) await this.verifyResolver(id);
    await this.call(["start", id]);
    if (this.resolverVolume) await this.verifyResolver(id);
    if (gated) {
      check(this.resolver, "resolver_missing");
      await this.verifyResolver(id);
      await this.audit(id);
      await this.verifyNetwork(true);
      if (host) await this.verifyHostImage(id);
      await this.call([
        "exec",
        id,
        "/bin/sh",
        "-ec",
        "umask 077; : > /tmp/mcp-start",
      ]);
      await this.verifyResolver(id);
    }
    return id;
  }
  private async verifyHostImage(id: string) {
    const files = [
      ["/usr/local/bin/engram-host-agent", this.inputs.binaries.hostAgent],
      ["/usr/local/bin/engram-uffd-handler", this.inputs.binaries.helper],
      ["/usr/local/bin/engram-egress-proxyd", this.inputs.proxy],
      ["/usr/local/bin/firecracker", this.inputs.firecracker],
    ] as const;
    for (const [path, pin] of files) {
      check(pin, "host_image_pin");
      await verifyPin(pin);
      const dir = join(
        this.root,
        `image-check-${randomBytes(8).toString("hex")}`,
      );
      await mkdir(dir, { mode: 0o700 });
      try {
        const target = join(dir, "binary");
        await this.call(["cp", `${id}:${path}`, target]);
        check(
          (await lstat(target)).isFile() &&
            hash(await readFile(target)) === pin.sha256,
          "host_image_binary",
        );
      } finally {
        await rm(dir, { recursive: true });
      }
    }
  }
  async audit(id: string) {
    const spec = this.specifications.get(id);
    check(spec, "container_specification");
    const actual = await this.inspect(id),
      h = actual.HostConfig,
      c = actual.Config;
    const values = (flag: string) =>
      spec.options.flatMap((v, i) =>
        v === flag ? [spec.options[i + 1]!] : [],
      );
    const one = (flag: string) => values(flag)[0];
    check(
      actual.Image === spec.image &&
        h.RestartPolicy.Name === "no" &&
        h.Privileged === false &&
        h.ReadonlyRootfs === spec.options.includes("--read-only") &&
        h.PidsLimit === (spec.host ? 1024 : 128) &&
        h.Memory === (spec.host ? 8589934592 : 805306368) &&
        h.NanoCpus === (spec.host ? 4000000000 : 2000000000),
      "container_resources",
    );
    check(
      c.User === (one("--user") ?? "") &&
        h.NetworkMode === one("--network") &&
        h.PidMode === "" &&
        h.IpcMode === "private" &&
        !h.UTSMode &&
        !h.UsernsMode &&
        (!h.CgroupnsMode || h.CgroupnsMode === "private"),
      "container_namespace",
    );
    check(
      JSON.stringify([...(h.CapDrop ?? [])].sort()) === '["ALL"]' &&
        JSON.stringify([...(h.CapAdd ?? [])].sort()) ===
          JSON.stringify(values("--cap-add").sort()) &&
        JSON.stringify(h.SecurityOpt ?? []) === '["no-new-privileges"]',
      "container_capabilities",
    );
    check(
      (h.Binds ?? []).length === 0 &&
        (h.VolumesFrom ?? []).length === 0 &&
        JSON.stringify(
          (h.Devices ?? [])
            .map(
              (v: {
                PathOnHost: string;
                PathInContainer: string;
                CgroupPermissions: string;
              }) =>
                `${v.PathOnHost}:${v.PathInContainer}:${v.CgroupPermissions}`,
            )
            .sort(),
        ) === JSON.stringify(values("--device").sort()) &&
        (h.DeviceRequests ?? []).length === 0,
      "container_authority",
    );
    const publications: Record<string, { HostIp: string; HostPort: string }[]> =
      {};
    for (const value of values("--publish")) {
      const [HostIp, HostPort, port, ...rest] = value.split(":");
      check(
        HostIp && HostPort && port && rest.length === 0,
        "publication_specification",
      );
      publications[`${port}/tcp`] = [{ HostIp, HostPort }];
    }
    check(
      JSON.stringify(Object.entries(h.PortBindings ?? {}).sort()) ===
        JSON.stringify(Object.entries(publications).sort()) &&
        !h.PublishAllPorts &&
        (h.ExtraHosts ?? []).length === 0 &&
        !h.CgroupParent &&
        Object.keys(h.Sysctls ?? {}).length === 0,
      "container_exposure",
    );
    check(
      JSON.stringify(h.Dns ?? []) === JSON.stringify(values("--dns")) &&
        JSON.stringify(h.DnsOptions ?? []) ===
          JSON.stringify(values("--dns-option")),
      "container_dns_configuration",
    );
    const mounts = values("--mount").map((value) =>
      Object.fromEntries(
        value
          .split(",")
          .map((part) =>
            part.includes("=") ? part.split("=", 2) : [part, "true"],
          ),
      ),
    );
    check(
      actual.Mounts.filter((m: { Type: string }) => m.Type !== "tmpfs")
        .length === mounts.length,
      "container_mount_count",
    );
    check(
      (h.Mounts ?? []).length === mounts.length,
      "container_mount_specification_count",
    );
    for (const m of mounts) {
      let volumeSource: string | undefined;
      const configured = (h.Mounts ?? []).filter(
        (entry: { Target: string }) => entry.Target === m.dst,
      );
      check(
        configured.length === 1 &&
          configured[0].Type === m.type &&
          configured[0].Source === m.src &&
          !!configured[0].ReadOnly === !!m.readonly &&
          (configured[0].VolumeOptions?.Subpath ?? "") ===
            (m["volume-subpath"] ?? "") &&
          !!configured[0].VolumeOptions?.NoCopy === !!m["volume-nocopy"],
        "container_mount_specification",
      );
      if (m.type === "volume") {
        check([...this.volumes.values()].includes(m.src!), "volume_scope");
        const [volume] = JSON.parse(
          (await this.call(["volume", "inspect", m.src!])).toString(),
        );
        check(
          volume?.Name === m.src &&
            volume?.Labels?.["engrams.mcp.run"] === this.runId &&
            volume.Driver === "local" &&
            volume.Scope === "local" &&
            Object.keys(volume.Options ?? {}).length === 0,
          "volume_identity",
        );
        volumeSource = volume.Mountpoint;
        check(
          typeof volumeSource === "string" && volumeSource.startsWith("/"),
          "volume_source",
        );
      }
      const found = actual.Mounts.filter(
        (entry: { Destination: string }) => entry.Destination === m.dst,
      );
      check(
        found.length === 1 &&
          found[0].Type === m.type &&
          found[0].RW === !m.readonly &&
          (m.type !== "volume" || found[0].Source === volumeSource) &&
          (m.type !== "bind" || found[0].Propagation === "rprivate") &&
          (m.type === "volume" ? found[0].Name : found[0].Source) === m.src,
        "container_mount",
      );
    }
    const expectedTmpfs = Object.fromEntries(
      values("--tmpfs").map((v) => {
        const i = v.indexOf(":");
        return [v.slice(0, i), v.slice(i + 1)];
      }),
    );
    check(
      actual.Mounts.filter((m: { Type: string }) => m.Type === "tmpfs").every(
        (m: { Destination: string }) =>
          Object.hasOwn(expectedTmpfs, m.Destination),
      ),
      "container_tmpfs_mounts",
    );
    check(
      JSON.stringify(Object.entries(h.Tmpfs ?? {}).sort()) ===
        JSON.stringify(Object.entries(expectedTmpfs).sort()),
      "container_tmpfs",
    );
    const [image] = JSON.parse(
      (await this.call(["image", "inspect", spec.image])).toString(),
    );
    check(image?.Id === spec.image, "container_image");
    const environment = new Map<string, string>();
    for (const entry of image.Config.Env ?? []) {
      const index = entry.indexOf("="),
        key = entry.slice(0, index),
        value = entry.slice(index + 1);
      check(
        [
          "PATH",
          "NODE_VERSION",
          "YARN_VERSION",
          "BUN_VERSION",
          "NODE_ENV",
          "PG_MAJOR",
          "PG_VERSION",
          "PG_SHA256",
          "PGDATA",
          "LANG",
          "GOSU_VERSION",
          "NGINX_VERSION",
          "NJS_VERSION",
          "NJS_RELEASE",
          "PKG_RELEASE",
          "DYNPKG_RELEASE",
          "ENGRAM_MKE2FS",
        ].includes(key),
        "image_environment",
      );
      environment.set(key, value);
    }
    for (const envFile of values("--env-file"))
      for (const line of (await readOwned(envFile))
        .toString()
        .split("\n")
        .filter(Boolean)) {
        const i = line.indexOf("=");
        check(i > 0, "environment_line");
        environment.set(line.slice(0, i), line.slice(i + 1));
      }
    for (const line of values("--env")) {
      const i = line.indexOf("=");
      check(i > 0, "environment_line");
      environment.set(line.slice(0, i), line.slice(i + 1));
    }
    check(
      JSON.stringify([...(c.Env ?? [])].sort()) ===
        JSON.stringify([...environment].map(([k, v]) => `${k}=${v}`).sort()),
      "container_environment",
    );
    check(
      JSON.stringify(c.Entrypoint ?? []) ===
        JSON.stringify(
          spec.entrypoint ? [spec.entrypoint] : (image.Config.Entrypoint ?? []),
        ) &&
        JSON.stringify(c.Cmd ?? []) ===
          JSON.stringify(
            spec.args.length ? spec.args : (image.Config.Cmd ?? []),
          ),
      "container_entrypoint",
    );
    return actual;
  }
  async verifyResolver(id: string) {
    check(this.resolver, "resolver_missing");
    await verifyPin(this.resolver);
    const directory = join(
      this.root,
      `resolver-${randomBytes(8).toString("hex")}`,
    );
    await mkdir(directory, { mode: 0o700 });
    try {
      const file = join(directory, "resolv.conf");
      await this.call(["cp", `${id}:/etc/resolv.conf`, file]);
      check(
        (await lstat(file)).isFile() &&
          hash(await readFile(file)) === this.resolver.sha256,
        "resolver_changed",
      );
    } finally {
      await rm(directory, { recursive: true });
    }
  }
  async reserveHostPort() {
    return this.reservePort(9101, this.inputs.nativeHostIp);
  }
  async reserveRegistryPort() {
    return this.reservePort(this.inputs.registryPort, "127.0.0.1");
  }
  private async reservePort(port: number, address: string) {
    const server = createServer();
    try {
      await new Promise<void>((resolve, reject) => {
        server.once("error", reject);
        server.listen(port, address, resolve);
      });
    } catch {
      throw new PlatformFailure(
        "NEEDS_CONTEXT",
        port === 9101 ? "host_port_occupied" : "registry_port_occupied",
      );
    } finally {
      if (server.listening)
        await new Promise<void>((resolve) => server.close(() => resolve()));
    }
  }
  async remove(role: string): Promise<void> {
    const id = this.containers.get(role);
    check(id, "owned_role");
    await this.inspect(id);
    const state = await this.inspect(id);
    if (state.State?.Running)
      await this.call(["stop", "--time", "3", id], undefined, 5000);
    check(
      (await this.inspect(id)).State?.Running === false &&
        (await this.inspect(id)).State?.Pid === 0,
      "container_stop",
    );
    await this.call(["rm", id]);
    const remaining = (
      await this.call(["ps", "-aq", "--no-trunc", "--filter", `id=${id}`])
    )
      .toString()
      .trim();
    check(!remaining, "container_survivor");
    this.specifications.delete(id);
    this.containers.delete(role);
  }
  async volume(role: string): Promise<string> {
    const name = this.name(role);
    const matches = (
      await this.call([
        "volume",
        "ls",
        "--filter",
        `name=${name}`,
        "--format",
        "{{.Name}}",
      ])
    )
      .toString()
      .split("\n");
    check(!matches.includes(name), "volume_exists");
    check(
      (await this.call(["volume", "create", "--label", this.label(), name]))
        .toString()
        .trim() === name,
      "volume_create",
    );
    this.volumes.set(role, name);
    return name;
  }
  async project(
    role: string,
    files: Record<string, Pin>,
    uid: 999 | 1000 = 1000,
    publicProjection = false,
  ): Promise<string> {
    for (const file of Object.values(files)) await verifyPin(file);
    const volume = await this.volume(role);
    const stager = `${role}-stage`;
    const id = await this.create(
      stager,
      this.inputs.images.guard.id,
      [
        "--network",
        "none",
        "--user",
        "0:0",
        "--cap-add",
        "CHOWN",
        "--cap-add",
        "DAC_OVERRIDE",
        "--cap-add",
        "FOWNER",
        "--mount",
        `type=volume,src=${volume},dst=/projection`,
      ],
      ["infinity"],
      "/bin/sleep",
    );
    for (const [name, file] of Object.entries(files)) {
      check(
        /^[a-zA-Z0-9_-][a-zA-Z0-9_./-]{0,160}$/.test(name) &&
          !name
            .split("/")
            .some((part) => part === ".." || part === "." || part === ""),
        "projection_file",
      );
      const parent = name.includes("/")
        ? name.slice(0, name.lastIndexOf("/"))
        : "";
      if (parent)
        await this.call([
          "exec",
          id,
          "/bin/mkdir",
          "-p",
          `/projection/${parent}`,
        ]);
      await this.call(["cp", file.path, `${id}:/projection/${name}`]);
    }
    await this.call([
      "exec",
      id,
      "/bin/sh",
      "-ec",
      `chown -R ${uid}:${uid} /projection; find /projection -type d -exec chmod ${publicProjection ? "755" : "700"} '{}' ';'; find /projection -type f -exec chmod ${publicProjection ? "644" : "600"} '{}' ';'`,
    ]);
    for (const [name, file] of Object.entries(files)) {
      const digest = (
        await this.call([
          "exec",
          id,
          "/usr/bin/sha256sum",
          `/projection/${name}`,
        ])
      )
        .toString()
        .split(/\s/)[0];
      check(digest === file.sha256, "projection_digest");
    }
    await this.remove(stager);
    return volume;
  }
  async network(): Promise<void> {
    this.resolver = await put(
      join(this.root, "resolv.conf"),
      `nameserver ${address(this.inputs, "dns")}\noptions ndots:1 timeout:1 attempts:1\n`,
    );
    const name = this.name("network");
    const matches = (
      await this.call([
        "network",
        "ls",
        "--filter",
        `name=${name}`,
        "--format",
        "{{.Name}}",
      ])
    )
      .toString()
      .split("\n");
    check(!matches.includes(name), "network_exists");
    this.plannedNetworkName = name;
    this.networkId = (
      await this.call([
        "network",
        "create",
        "--internal",
        "--driver",
        "bridge",
        "--subnet",
        this.inputs.subnet,
        "--label",
        this.label(),
        name,
      ])
    )
      .toString()
      .trim();
    check(/^[a-f0-9]{64}$/.test(this.networkId), "network_id");
    const [actual] = JSON.parse(
      (await this.call(["network", "inspect", this.networkId])).toString(),
    );
    check(
      actual?.Id === this.networkId &&
        actual?.Internal === true &&
        actual?.Driver === "bridge" &&
        actual?.Labels?.["engrams.mcp.run"] === this.runId &&
        actual?.IPAM?.Config?.length === 1 &&
        actual.IPAM.Config[0].Subnet === this.inputs.subnet,
      "network_state",
    );
    this.resolverVolume = await this.project(
      "resolver",
      { "resolv.conf": this.resolver },
      1000,
      true,
    );
  }
  networkName() {
    check(this.networkId, "network_missing");
    return this.networkId;
  }
  networkOptions(role: keyof typeof import("./network.ts").ADDRESS): string[] {
    check(this.networkId, "network_missing");
    return [
      "--network",
      this.networkId,
      "--ip",
      address(this.inputs, role),
      "--dns",
      address(this.inputs, "dns"),
      "--dns-option",
      "attempts:1",
      "--dns-option",
      "timeout:1",
    ];
  }
  async guards(ca: Pin): Promise<void> {
    const trust = await this.project(
      "probe-trust",
      { "ca.pem": ca },
      1000,
      true,
    );
    for (const role of GUARDS) {
      const id = await this.create(
        `${role}-guard`,
        this.inputs.images.guard.id,
        [
          ...this.networkOptions(role),
          "--mount",
          `type=volume,src=${trust},dst=/etc/mcp-probe,readonly`,
          ...(role === "coordinator"
            ? [
                "--publish",
                `127.0.0.1:${this.inputs.registryPort}:${this.inputs.registryPort}`,
                ...(this.inputs.backend === "firecracker"
                  ? ["--publish", `${this.inputs.nativeHostIp}:9101:9101`]
                  : []),
              ]
            : []),
          "--read-only",
          "--user",
          "0:0",
          "--cap-add",
          "NET_ADMIN",
          "--tmpfs",
          "/tmp:rw,noexec,nosuid,nodev,size=4m",
        ],
        ["infinity"],
        "/bin/sleep",
      );
      this.guardIds.set(role, id);
      if (role === "coordinator") {
        const guard = await this.inspect(id);
        const bindings = guard.HostConfig.PortBindings;
        check(
          Object.keys(bindings).length ===
            (this.inputs.backend === "firecracker" ? 2 : 1) &&
            JSON.stringify(bindings[`${this.inputs.registryPort}/tcp`]) ===
              JSON.stringify([
                {
                  HostIp: "127.0.0.1",
                  HostPort: String(this.inputs.registryPort),
                },
              ]),
          "registry_publication",
        );
        if (this.inputs.backend === "firecracker")
          check(
            JSON.stringify(bindings["9101/tcp"]) ===
              JSON.stringify([
                { HostIp: this.inputs.nativeHostIp, HostPort: "9101" },
              ]),
            "host_publication",
          );
      }
      const plan = rules(this.inputs, role);
      await this.call(
        ["exec", "-i", id, "/usr/sbin/nft", "-j", "-f", "-"],
        JSON.stringify({ nftables: plan.map((item) => ({ add: item })) }),
      );
    }
    await this.verifyNetwork(false);
  }
  async verifyNetwork(probes: boolean) {
    check(this.guardIds.size === 3 && this.networkId, "guard_set");
    const results: string[] = [];
    const [network] = JSON.parse(
      (await this.call(["network", "inspect", this.networkId])).toString(),
    );
    check(
      network?.Id === this.networkId &&
        network.Internal === true &&
        network.Labels?.["engrams.mcp.run"] === this.runId,
      "network_changed",
    );
    for (const role of GUARDS) {
      const id = this.guardIds.get(role)!;
      const guard = await this.audit(id);
      await this.verifyResolver(id);
      check(
        guard.State?.Running === true &&
          guard.NetworkSettings?.Networks?.[this.name("network")]?.IPAddress ===
            address(this.inputs, role),
        "guard_identity",
      );
      results.push(
        effectiveRules(
          JSON.parse(
            (
              await this.call([
                "exec",
                id,
                "/usr/sbin/nft",
                "-j",
                "list",
                "table",
                "inet",
                "mcp",
              ])
            ).toString(),
          ),
          rules(this.inputs, role),
        ),
      );
      if (probes) {
        // The owned fixture is reachable. The owned denial sink is live but not allowed.
        for (const fixture of ["modern", "legacy"] as const) {
          await this.call([
            "exec",
            id,
            "/usr/bin/nc",
            "-z",
            "-w",
            "2",
            address(this.inputs, fixture),
            "8443",
          ]);
          await this.call([
            "exec",
            id,
            "/usr/bin/curl",
            "--silent",
            "--show-error",
            "--max-time",
            "2",
            "--cacert",
            "/etc/mcp-probe/ca.pem",
            "--output",
            "/dev/null",
            `https://resource.${fixture}.fixture.example:8443/`,
          ]);
        }
        // No external packet is sent: both probes target the owned DNS namespace or the blocked embedded stub.
        const dns = await this.call([
          "exec",
          id,
          "/usr/bin/dig",
          "+time=1",
          "+tries=1",
          "+short",
          "resource.modern.fixture.example",
          "A",
        ]);
        check(
          dns.toString().trim() === address(this.inputs, "modern"),
          "fixture_dns",
        );
        const unknown = await this.call([
          "exec",
          id,
          "/usr/bin/dig",
          "+time=1",
          "+tries=1",
          "+comments",
          `@${address(this.inputs, "dns")}`,
          "unrouted.invalid",
          "A",
        ]);
        check(
          /status: (NXDOMAIN|SERVFAIL),/.test(unknown.toString()) &&
            /ANSWER: 0,/.test(unknown.toString()),
          "external_dns",
        );
        // Start the three fixed negative probes together, then wait for all of
        // them. Their one-second bounds also keep restart controls bounded.
        const negative = await this.call([
          "exec",
          id,
          "/bin/sh",
          "-ec",
          `
          deny() { if /usr/bin/nc -z -w 1 ${address(this.inputs, "denied")} 8080; then return 1; else code=$?; test "$code" = 1; fi; }
          block() { if /usr/bin/dig +time=1 +tries=1 "@$1" unrouted.invalid A >/dev/null 2>&1; then return 1; else code=$?; test "$code" = 9; fi; }
          deny & first=$!; block 127.0.0.11 & second=$!; block ${address(this.inputs, "denied")} & third=$!
          failed=0; wait "$first" || failed=1; wait "$second" || failed=1; wait "$third" || failed=1
          test "$failed" = 0; printf blocked
        `,
        ]);
        check(negative.toString() === "blocked", "negative_network_probes");
      }
    }
    const enforcementSha256 = hash(results.join("\n"));
    if (this.baseline)
      check(this.baseline === enforcementSha256, "network_generation");
    else this.baseline = enforcementSha256;
    return {
      generation: this.generation,
      enforcementSha256,
      fixtureOnlyEgress: true as const,
      externalDnsBlocked: true as const,
      roles: GUARDS,
    };
  }
  async qualifyTools(): Promise<void> {
    const versions = JSON.parse(
      (await this.call(["version", "--format", "{{json .}}"])).toString(),
    );
    for (const selected of [versions.Client, versions.Server]) {
      const api = /^1\.([0-9]+)$/.exec(selected?.ApiVersion ?? "");
      check(
        /^[0-9]+\./.test(selected?.Version ?? "") &&
          Number(selected.Version.split(".")[0]) >= 26 &&
          api &&
          Number(api[1]) >= 45,
        "engine_volume_subpath_support",
      );
    }
    const id = await this.create(
      "tool-check",
      this.inputs.images.guard.id,
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
    try {
      await this.call([
        "exec",
        id,
        "/bin/sh",
        "-ec",
        'for tool in /bin/sh /bin/bash /bin/sleep /bin/mkdir /bin/chown /bin/chmod /usr/bin/find /usr/bin/sha256sum /usr/bin/skopeo /usr/sbin/nft /usr/bin/dig /usr/bin/nc /usr/bin/curl /usr/bin/mksquashfs; do test -x "$tool"; done',
      ]);
      const version = (
        await this.call(["exec", id, "/usr/bin/skopeo", "--version"])
      ).toString();
      check(/^skopeo version [0-9]+\./.test(version), "guard_skopeo");
    } finally {
      await this.remove("tool-check");
    }
  }
  async nativeDns(): Promise<void> {
    for (const [name, expected] of Object.entries(dnsRecords(this.inputs))) {
      const actual = await lookup(name, { all: true });
      if (!actual.length || actual.some((entry) => entry.address !== expected))
        throw new PlatformFailure("NEEDS_CONTEXT", "native_dns_route");
    }
  }
  async cleanup(): Promise<void> {
    let failed = false;
    const stopped: string[] = [];
    // A cancelled CLI can lose a successful daemon create response. Recover
    // only exact planned names with this run's label before stopping anything.
    try {
      const ids = (
        await this.call([
          "ps",
          "-aq",
          "--no-trunc",
          "--filter",
          `label=${this.label()}`,
        ])
      )
        .toString()
        .trim()
        .split("\n")
        .filter(Boolean);
      for (const id of ids) {
        check(/^[a-f0-9]{64}$/.test(id), "cleanup_container_id");
        if ([...this.containers.values()].includes(id)) continue;
        const actual = await this.inspect(id);
        const role = this.plannedContainers.get(
          String(actual.Name).replace(/^\//, ""),
        );
        check(
          role && !this.containers.has(role),
          "cleanup_unplanned_container",
        );
        this.containers.set(role, id);
      }
    } catch {
      failed = true;
    }
    for (const [role] of [...this.containers].reverse()) {
      try {
        const id = this.containers.get(role)!;
        const logs = await this.call(["logs", "--tail", "10000", id]);
        await put(join(this.root, `retained-${role}.log`), logs);
      } catch {
        failed = true;
      }
      try {
        await this.remove(role);
        stopped.push(role);
      } catch {
        failed = true;
      }
    }
    if (!this.networkId && this.plannedNetworkName) {
      try {
        const ids = (
          await this.call([
            "network",
            "ls",
            "--no-trunc",
            "--filter",
            `label=${this.label()}`,
            "--filter",
            `name=${this.plannedNetworkName}`,
            "--format",
            "{{.ID}}",
          ])
        )
          .toString()
          .trim()
          .split("\n")
          .filter(Boolean);
        check(
          ids.length <= 1 && ids.every((id) => /^[a-f0-9]{64}$/.test(id)),
          "cleanup_network_identity",
        );
        this.networkId = ids[0];
      } catch {
        failed = true;
      }
    }
    if (this.networkId && this.containers.size === 0) {
      try {
        const [actual] = JSON.parse(
          (await this.call(["network", "inspect", this.networkId])).toString(),
        );
        check(
          actual?.Id === this.networkId &&
            actual?.Labels?.["engrams.mcp.run"] === this.runId &&
            Object.keys(actual.Containers ?? {}).length === 0,
          "network_cleanup",
        );
        await this.call(["network", "rm", this.networkId]);
        this.networkId = undefined;
      } catch {
        failed = true;
      }
    }
    // Named volumes retain database and effect history. The report records them; they are not active resources.
    const survivors = (
      await this.call(["ps", "-aq", "--filter", `label=${this.label()}`]).catch(
        () => Buffer.from("unknown"),
      )
    )
      .toString()
      .trim();
    if (survivors || this.networkId) failed = true;
    if (this.plannedNetworkName) {
      const remaining = await this.call([
        "network",
        "ls",
        "--no-trunc",
        "--filter",
        `label=${this.label()}`,
        "--format",
        "{{.ID}}",
      ]).catch(() => Buffer.from("unknown"));
      if (remaining.toString().trim()) failed = true;
    }
    await put(
      join(this.root, "cleanup.json"),
      JSON.stringify({
        version: 1,
        runId: this.runId,
        stopped,
        retainedVolumes: [...this.volumes.values()],
        containersRemaining: this.containers.size,
        networkRemaining: !!this.networkId,
        success: !failed,
      }),
    );
    if (failed) throw new PlatformFailure("FAILED", "engine_cleanup");
  }
}
