import { FcRuntime, type FcInventory } from "./fc-runtime.ts";
import { randomBytes, X509Certificate } from "node:crypto";
import { lookup } from "node:dns/promises";
import type { LookupAddress } from "node:dns";
import { lstat, mkdir, readFile, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { join } from "node:path";
import { networkInterfaces } from "node:os";
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

type NativeRoute = { gateway: string; dev: string; source: string };
const ipv4 = (value: string) => {
  const parts = value.split(".").map(Number);
  check(
    parts.length === 4 &&
      parts.every((v) => Number.isInteger(v) && v >= 0 && v <= 255) &&
      parts.join(".") === value,
    "native_link_ip",
  );
  return parts.reduce((a, v) => a * 256 + v, 0);
};
export function verifyNativeSubnet(
  subnet: string,
  gateway: string,
  forbidden: string[],
) {
  const [ip, prefix, ...rest] = subnet.split("/");
  check(
    ip && prefix && !rest.length && /^(?:[1-9]|[12][0-9]|30)$/.test(prefix),
    "native_link_subnet",
  );
  const first = ipv4(ip),
    count = 2 ** (32 - Number(prefix));
  check(
    first % count === 0 &&
      ((first >= ipv4("10.0.0.0") && first + count <= ipv4("11.0.0.0")) ||
        (first >= ipv4("172.16.0.0") && first + count <= ipv4("172.32.0.0")) ||
        (first >= ipv4("192.168.0.0") && first + count <= ipv4("192.169.0.0"))),
    "native_link_private",
  );
  check(
    ipv4(gateway) > first && ipv4(gateway) < first + count - 1,
    "native_link_gateway",
  );
  for (const entry of forbidden) {
    const [address, bits = "32"] = entry.split("/");
    const start = ipv4(address!),
      end = start + 2 ** (32 - Number(bits));
    check(first + count <= start || first >= end, "native_link_overlap");
  }
}
export function nativeInterface(
  value: unknown,
  address: string,
  mac: string,
): string {
  check(
    Array.isArray(value) && /^[a-f0-9]{2}(?::[a-f0-9]{2}){5}$/.test(mac),
    "native_link_shape",
  );
  const matches = value.filter(
    (v) =>
      v &&
      v.address === mac &&
      Array.isArray(v.addr_info) &&
      v.addr_info.some(
        (a: { family?: string; local?: string }) =>
          a.family === "inet" && a.local === address,
      ),
  );
  check(
    matches.length === 1 && /^[a-zA-Z0-9_-]{1,15}$/.test(matches[0].ifname),
    "native_link_interface",
  );
  return matches[0].ifname;
}
export function verifyNativeRouteState(
  value: unknown,
  selected: unknown,
  inputs: Pick<Inputs, "subnet" | "nativeHostIp">,
  link: NativeRoute = {
    gateway: inputs.subnet.slice(0, -4) + "1",
    dev: "eth0",
    source: address(inputs, "coordinator"),
  },
) {
  check(
    Array.isArray(value) && Array.isArray(selected) && selected.length === 1,
    "native_route_shape",
  );
  const objects = value.map((route: unknown) => {
    check(
      route !== null && typeof route === "object" && !Array.isArray(route),
      "native_route_shape",
    );
    return route as Record<string, unknown>;
  });
  check(
    !objects.some(
      (route) => route.dst === "default" || route.dst === "0.0.0.0/0",
    ),
    "native_route_default",
  );
  const exact = objects.filter(
    (route) =>
      route.dst === inputs.nativeHostIp ||
      route.dst === `${inputs.nativeHostIp}/32`,
  );
  const { gateway } = link;
  check(
    exact.length === 1 &&
      exact[0]!.gateway === gateway &&
      exact[0]!.dev === link.dev &&
      !exact[0]!.multipath &&
      (!exact[0]!.type || exact[0]!.type === "unicast"),
    "native_route_exact",
  );
  const route = selected[0];
  check(
    route !== null &&
      typeof route === "object" &&
      !Array.isArray(route) &&
      route.dst === inputs.nativeHostIp &&
      route.gateway === gateway &&
      route.dev === link.dev &&
      route.prefsrc === link.source &&
      !route.multipath,
    "native_route_selected",
  );
}
export function verifyNativeHostTls(
  output: Uint8Array,
  expectedFingerprint: string,
) {
  const text = Buffer.from(output).toString();
  check(
    output.length <= 65536 &&
      /(?:New,|Protocol\s*:)\s*TLSv1\.3\b/.test(text) &&
      /Verify return code: 0 \(ok\)/.test(text),
    "native_host_tls_verification",
  );
  const certificate =
    /-----BEGIN CERTIFICATE-----[\s\S]*?-----END CERTIFICATE-----/.exec(text);
  check(
    certificate &&
      new X509Certificate(certificate[0]).fingerprint256 ===
        expectedFingerprint,
    "native_host_tls_leaf",
  );
  return hash(new X509Certificate(certificate[0]).raw);
}
type NativeHostTls = {
  ca: Pin;
  clientCertificate: Pin;
  clientKey: Pin;
  serverCertificate: Pin;
};

// Match the fixed production host_startup rules in net.rs. Per-VM SNAT
// rules live in each VM's own netns and must not appear in these chains.
export function fcHostFirewall(port: number): Record<string, string[]> {
  const pool = "10.200.0.0/16";
  const rule = (
    chain: string,
    match: string,
    action: string,
    comment: string,
  ) => `-A ${chain} ${match} -j ${action} -m comment --comment ${comment}`;
  const input = (protocol: string, p: number, comment: string) =>
    rule("INPUT", `-s ${pool} -p ${protocol} --dport ${p}`, "ACCEPT", comment);
  const redirect = (
    i: string,
    protocol: string,
    p: number,
    to: number,
    comment: string,
    dest = "",
  ) =>
    rule(
      "PREROUTING",
      `-i ${i} -p ${protocol} ${dest} --dport ${p}`,
      `REDIRECT --to-port ${to}`,
      comment,
    );
  return {
    INPUT: [
      input("tcp", port + 3, "engram-guest-gateway-input"),
      input("tcp", port + 2, "engram-proxy-dns-input"),
      input("udp", port + 2, "engram-proxy-dns-input"),
      input("tcp", port + 1, "engram-proxy-input"),
      rule(
        "INPUT",
        `-s ${pool} -m conntrack --ctstate ESTABLISHED,RELATED`,
        "ACCEPT",
        "engram-host-input-established",
      ),
      rule("INPUT", `-s ${pool}`, "DROP", "engram-host-input"),
    ],
    FORWARD: [
      rule("FORWARD", `-s ${pool} -d ${pool}`, "DROP", "engram-isolate-vm-vm"),
      ...[
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "169.254.0.0/16",
        "127.0.0.0/8",
      ].map((net) =>
        rule("FORWARD", `-s ${pool} -d ${net}`, "DROP", "engram-host-lan"),
      ),
      rule("FORWARD", `-s ${pool}`, "DROP", "engram-default-deny"),
    ],
    PREROUTING: [
      ...["vh-engr-+", "tap-engr-+"].map((i) =>
        redirect(
          i,
          "tcp",
          80,
          port + 3,
          "engram-guest-gateway-redirect",
          "-d 169.254.169.254/32",
        ),
      ),
      ...["tap-engr-+", "vh-engr-+"].map((i) =>
        redirect(i, "tcp", 443, port + 1, "engram-proxy-redirect"),
      ),
      ...["udp", "tcp"].flatMap((proto) =>
        ["tap-engr-+", "vh-engr-+"].map((i) =>
          redirect(i, proto, 53, port + 2, "engram-dns-redirect"),
        ),
      ),
    ],
    POSTROUTING: [
      rule(
        "POSTROUTING",
        `-s ${pool} ! -d ${pool}`,
        "MASQUERADE",
        "engram-masq",
      ),
    ],
  };
}
function firewallRule(line: string): string {
  const tokens = line.match(/"[^"\r\n]*"|[^\s]+/g) ?? [];
  const fields: Record<string, string> = {};
  let negate = false;
  for (let i = 0; i < tokens.length; i++) {
    let key = tokens[i]!;
    if (key === "!") {
      check(!negate, "fc_firewall_rule");
      negate = true;
      continue;
    }
    const value = tokens[++i]?.replace(/^"|"$/g, "");
    check((value && !negate) || (value && key === "-d"), "fc_firewall_rule");
    if (key === "-m") {
      check(
        ["comment", "tcp", "udp", "conntrack"].includes(value!),
        "fc_firewall_rule",
      );
      continue;
    }
    if (key === "--to-ports") key = "--to-port";
    check(
      [
        "-A",
        "-s",
        "-d",
        "-i",
        "-p",
        "--dport",
        "-j",
        "--to-port",
        "--ctstate",
        "--comment",
      ].includes(key),
      "fc_firewall_rule",
    );
    if (negate) key = "!" + key;
    check(!(key in fields), "fc_firewall_rule");
    fields[key] =
      key === "--ctstate" ? value!.split(",").sort().join(",") : value!;
    negate = false;
  }
  check(!negate, "fc_firewall_rule");
  return JSON.stringify(Object.entries(fields).sort());
}
export function verifyFcHostFirewall(
  actual: Record<string, string>,
  port: number,
) {
  const expected = fcHostFirewall(port);
  check(
    Object.keys(actual).sort().join(",") ===
      Object.keys(expected).sort().join(","),
    "fc_firewall_chains",
  );
  for (const [chain, lines] of Object.entries(expected)) {
    const current = actual[chain]!.trim().split("\n");
    check(current.shift() === `-P ${chain} ACCEPT`, "fc_firewall_policy");
    check(
      JSON.stringify(current.map(firewallRule)) ===
        JSON.stringify(lines.map(firewallRule)),
      "fc_firewall_changed",
    );
  }
}
export const fcGuardSysctls = (
  backend: Inputs["backend"],
  role: string,
): Record<string, string> =>
  backend === "firecracker" && role === "coordinator-guard"
    ? { "net.ipv4.ip_forward": "1", "net.ipv4.conf.all.route_localnet": "1" }
    : {};

/** Fixed per-run Linux resources. No application request reaches these commands. */
export class Engine {
  private readonly containers = new Map<string, string>();
  private readonly plannedContainers = new Map<string, string>();
  private readonly volumes = new Map<string, string>();
  private networkId: string | undefined;
  private plannedNetworkName: string | undefined;
  private nativeNetworkId: string | undefined;
  private nativeNetworkName: string | undefined;
  private nativeLink:
    | (NativeRoute & { subnet: string; mac: string })
    | undefined;
  readonly guardIds = new Map<GuardRole, string>();
  private readonly generation = randomBytes(16).toString("hex");
  private baseline: string | undefined;
  private configState:
    | Awaited<ReturnType<typeof validateEngineConfig>>
    | undefined;
  private socketState: { ino: number; dev: number } | undefined;
  private resolver: Pin | undefined;
  private resolverVolume: string | undefined;
  private nativeHostProjection:
    | { volume: string; identity: string }
    | undefined;
  private readonly specifications = new Map<
    string,
    {
      image: string;
      options: string[];
      args: string[];
      entrypoint?: string;
      host: boolean;
      role: string;
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
  readonly fc: FcRuntime | undefined;
  constructor(
    readonly inputs: Inputs,
    readonly root: string,
    readonly runId: string,
    inventory?: FcInventory,
  ) {
    this.fc = inventory ? new FcRuntime(inventory, inputs, root) : undefined;
  }
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
      role,
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
      if (host) {
        await this.verifyHostImage(id);
        check(this.fc, "fc_inventory_missing");
        const actual = await this.inspect(id);
        await this.fc.qualifyKernel(actual.State.Pid);
        for (const [path, expected] of [
          ["/proc/sys/kernel/random/boot_id", this.fc.inventory.bootId],
          ["/proc/sys/kernel/osrelease", this.fc.inventory.release],
        ] as const)
          check(
            (await this.call(["exec", id, "/bin/cat", path]))
              .toString()
              .trim() === expected,
            "fc_engine_kernel_identity",
          );
        check(
          (
            await this.call([
              "exec",
              id,
              "/usr/bin/readlink",
              "/proc/self/ns/pid",
            ])
          )
            .toString()
            .trim() === this.fc.inventory.pidNamespace,
          "fc_engine_pid_namespace",
        );
      }
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
        h.PidMode ===
          (spec.host && this.inputs.backend === "firecracker" ? "host" : "") &&
        h.IpcMode === "private" &&
        !h.UTSMode &&
        !h.UsernsMode &&
        (spec.host
          ? h.CgroupnsMode === "private"
          : !h.CgroupnsMode || h.CgroupnsMode === "private"),
      "container_namespace",
    );
    const addedCapabilities = values("--cap-add").sort();
    check(
      (spec.role === "dns" || spec.role === "denied-dns"
        ? JSON.stringify(addedCapabilities) === '["NET_BIND_SERVICE"]'
        : !addedCapabilities.includes("NET_BIND_SERVICE")) &&
        JSON.stringify([...(h.CapDrop ?? [])].sort()) === '["ALL"]' &&
        JSON.stringify([...(h.CapAdd ?? [])].sort()) ===
          JSON.stringify(addedCapabilities) &&
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
        JSON.stringify(Object.entries(h.Sysctls ?? {}).sort()) ===
          JSON.stringify(
            Object.entries(
              fcGuardSysctls(this.inputs.backend, spec.role),
            ).sort(),
          ),
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
    const directoryMode = publicProjection ? "755" : "700";
    const fileMode = publicProjection ? "644" : "600";
    const directories = new Set(["/projection"]);
    for (const name of Object.keys(files)) {
      const parts = name.split("/");
      parts.pop();
      for (let i = 1; i <= parts.length; i++)
        directories.add(`/projection/${parts.slice(0, i).join("/")}`);
    }
    for (const directory of directories)
      check(
        (
          await this.call([
            "exec",
            id,
            "/usr/bin/stat",
            "-c",
            "%u:%g:%a:%F",
            directory,
          ])
        )
          .toString()
          .trim() === `${uid}:${uid}:${directoryMode}:directory`,
        "projection_directory_mode",
      );
    for (const name of Object.keys(files))
      check(
        (
          await this.call([
            "exec",
            id,
            "/usr/bin/stat",
            "-c",
            "%u:%g:%a:%F:%h",
            `/projection/${name}`,
          ])
        )
          .toString()
          .trim() === `${uid}:${uid}:${fileMode}:regular file:1`,
        "projection_file_mode",
      );
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
        "--gateway",
        this.inputs.subnet.slice(0, -4) + "1",
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
        actual.IPAM.Config[0].Subnet === this.inputs.subnet &&
        actual.IPAM.Config[0].Gateway === this.inputs.subnet.slice(0, -4) + "1",
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
          ...Object.entries(
            fcGuardSysctls(this.inputs.backend, `${role}-guard`),
          ).flatMap(([key, value]) => ["--sysctl", `${key}=${value}`]),
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
      if (role === "coordinator" && this.inputs.backend === "firecracker")
        await this.call([
          "exec",
          id,
          "/usr/sbin/ip",
          "-4",
          "route",
          "replace",
          `${this.inputs.nativeHostIp}/32`,
          "via",
          this.inputs.subnet.slice(0, -4) + "1",
          "dev",
          "eth0",
        ]);
    }
    if (this.inputs.backend === "vz") await this.connectNativeHost();
    await this.verifyNetwork(false);
  }
  private async connectNativeHost() {
    const id = this.guardIds.get("coordinator");
    check(id, "native_route_guard");
    const name = this.name("native-network");
    const existing = (
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
    check(!existing.includes(name), "native_network_exists");
    this.nativeNetworkName = name;
    this.nativeNetworkId = (
      await this.call([
        "network",
        "create",
        "--driver",
        "bridge",
        "--ipv6=false",
        "--label",
        this.label(),
        name,
      ])
    )
      .toString()
      .trim();
    check(/^[a-f0-9]{64}$/.test(this.nativeNetworkId), "native_network_id");
    const [network] = JSON.parse(
      (
        await this.call(["network", "inspect", this.nativeNetworkId])
      ).toString(),
    );
    check(
      network?.Id === this.nativeNetworkId &&
        network.Internal === false &&
        network.Driver === "bridge" &&
        network.EnableIPv6 === false &&
        network.Labels?.["engrams.mcp.run"] === this.runId &&
        network.IPAM?.Config?.length === 1 &&
        Object.keys(network.Containers ?? {}).length === 0,
      "native_network_state",
    );
    const { Subnet: subnet, Gateway: gateway } = network.IPAM.Config[0];
    check(
      typeof subnet === "string" && typeof gateway === "string",
      "native_network_ipam",
    );
    verifyNativeSubnet(subnet, gateway, [
      this.inputs.subnet,
      this.inputs.nativeHostIp,
      ...Object.values(networkInterfaces())
        .flat()
        .filter((v) => v?.family === "IPv4")
        .map((v) => v!.address),
    ]);
    await this.call(["network", "connect", this.nativeNetworkId, id]);
    const [connected] = JSON.parse(
      (
        await this.call(["network", "inspect", this.nativeNetworkId])
      ).toString(),
    );
    const member = connected.Containers?.[id];
    check(
      Object.keys(connected.Containers ?? {}).join(",") === id &&
        member &&
        typeof member.IPv4Address === "string" &&
        typeof member.MacAddress === "string",
      "native_network_membership",
    );
    const source = member.IPv4Address.split("/")[0];
    const interfaces = JSON.parse(
      (
        await this.call([
          "exec",
          id,
          "/usr/sbin/ip",
          "-j",
          "-4",
          "address",
          "show",
        ])
      ).toString(),
    );
    const dev = nativeInterface(interfaces, source, member.MacAddress);
    this.nativeLink = { subnet, gateway, source, mac: member.MacAddress, dev };
    // nft default-drop is already installed. Remove only this link's default before any app can share the namespace.
    const routes = JSON.parse(
      (
        await this.call([
          "exec",
          id,
          "/usr/sbin/ip",
          "-j",
          "-4",
          "route",
          "show",
          "default",
        ])
      ).toString(),
    );
    check(Array.isArray(routes) && routes.length <= 1, "native_link_default");
    for (const route of routes) {
      check(
        route.dst === "default" &&
          route.gateway === gateway &&
          route.dev === dev,
        "native_link_default",
      );
      await this.call([
        "exec",
        id,
        "/usr/sbin/ip",
        "-4",
        "route",
        "del",
        "default",
        "via",
        gateway,
        "dev",
        dev,
      ]);
    }
    await this.call([
      "exec",
      id,
      "/usr/sbin/ip",
      "-4",
      "route",
      "replace",
      `${this.inputs.nativeHostIp}/32`,
      "via",
      gateway,
      "dev",
      dev,
    ]);
  }
  async verifyNativeRoute() {
    const id = this.guardIds.get("coordinator");
    check(id, "native_route_guard");
    const routes = JSON.parse(
      (
        await this.call([
          "exec",
          id,
          "/usr/sbin/ip",
          "-j",
          "-4",
          "route",
          "show",
          "table",
          "all",
        ])
      ).toString(),
    );
    const selected = JSON.parse(
      (
        await this.call([
          "exec",
          id,
          "/usr/sbin/ip",
          "-j",
          "-4",
          "route",
          "get",
          this.inputs.nativeHostIp,
        ])
      ).toString(),
    );
    if (this.inputs.backend === "vz") {
      const link = this.nativeLink;
      check(
        link && this.nativeNetworkId && this.nativeNetworkName,
        "native_link_missing",
      );
      const [network] = JSON.parse(
        (
          await this.call(["network", "inspect", this.nativeNetworkId])
        ).toString(),
      );
      check(
        network?.Id === this.nativeNetworkId &&
          network.Name === this.nativeNetworkName &&
          network.Internal === false &&
          network.Driver === "bridge" &&
          network.EnableIPv6 === false &&
          network.Labels?.["engrams.mcp.run"] === this.runId &&
          network.IPAM?.Config?.length === 1 &&
          network.IPAM.Config[0].Subnet === link.subnet &&
          network.IPAM.Config[0].Gateway === link.gateway &&
          Object.keys(network.Containers ?? {}).join(",") === id &&
          network.Containers[id].IPv4Address.split("/")[0] === link.source &&
          network.Containers[id].MacAddress === link.mac,
        "native_network_changed",
      );
      const interfaces = JSON.parse(
        (
          await this.call([
            "exec",
            id,
            "/usr/sbin/ip",
            "-j",
            "-4",
            "address",
            "show",
          ])
        ).toString(),
      );
      check(
        nativeInterface(interfaces, link.source, link.mac) === link.dev,
        "native_link_changed",
      );
      verifyNativeRouteState(routes, selected, this.inputs, link);
    } else verifyNativeRouteState(routes, selected, this.inputs);
  }
  async qualifyFcHost(id: string) {
    check(this.inputs.backend === "firecracker", "fc_host_backend");
    const guard = this.guardIds.get("coordinator");
    check(guard, "fc_host_guard");
    const namespace = async (container: string) =>
      (
        await this.call([
          "exec",
          container,
          "/usr/bin/readlink",
          "/proc/self/ns/net",
        ])
      )
        .toString()
        .trim();
    const expected = await namespace(guard);
    check(
      /^net:\[[0-9]+\]$/.test(expected) && (await namespace(id)) === expected,
      "fc_host_namespace",
    );
    for (const container of [guard, id])
      for (const path of [
        "/proc/sys/net/ipv4/ip_forward",
        "/proc/sys/net/ipv4/conf/all/route_localnet",
      ])
        check(
          (
            await this.call(["exec", container, "/bin/cat", path])
          ).toString() === "1\n",
          "fc_host_sysctl",
        );
    const actual: Record<string, string> = {};
    for (const chain of ["INPUT", "FORWARD", "PREROUTING", "POSTROUTING"])
      actual[chain] = (
        await this.call([
          "exec",
          id,
          "/usr/sbin/iptables",
          "-t",
          ["INPUT", "FORWARD"].includes(chain) ? "filter" : "nat",
          "-S",
          chain,
        ])
      ).toString();
    verifyFcHostFirewall(actual, this.inputs.registryPort);
    await put(
      join(
        this.root,
        `fc-host-firewall-${randomBytes(8).toString("hex")}.json`,
      ),
      JSON.stringify({
        netns: expected,
        sysctls: fcGuardSysctls(this.inputs.backend, "coordinator-guard"),
        sha256: hash(JSON.stringify(actual)),
      }),
    );
  }
  async qualifyNativeHost(tls: NativeHostTls) {
    for (const pin of Object.values(tls)) {
      await verifyPin(pin);
      await readOwned(pin.path);
    }
    await this.verifyNetwork(false);
    const identity = hash(
      Object.values(tls)
        .map((pin) => pin.sha256)
        .join("\n"),
    );
    if (this.nativeHostProjection)
      check(
        this.nativeHostProjection.identity === identity,
        "native_host_probe_identity",
      );
    else
      this.nativeHostProjection = {
        identity,
        volume: await this.project("host-probe-tls", {
          "ca.pem": tls.ca,
          "client.crt": tls.clientCertificate,
          "client.key": tls.clientKey,
        }),
      };
    const role = "host-tls-probe";
    const expected = new X509Certificate(
      await readOwned(tls.serverCertificate.path),
    ).fingerprint256;
    try {
      const id = await this.create(
        role,
        this.inputs.images.guard.id,
        [
          "--network",
          `container:${this.guardIds.get("coordinator")}`,
          "--user",
          "1000:1000",
          "--read-only",
          "--mount",
          `type=volume,src=${this.nativeHostProjection.volume},dst=/etc/mcp-host-probe,readonly`,
          "--tmpfs",
          "/tmp:rw,noexec,nosuid,nodev,size=4m",
        ],
        ["infinity"],
        "/bin/sleep",
      );
      const output = await this.call(
        [
          "exec",
          id,
          "/usr/bin/openssl",
          "s_client",
          "-connect",
          "host.fixture.example:9101",
          "-servername",
          "host.fixture.example",
          "-verify_hostname",
          "host.fixture.example",
          "-verify_return_error",
          "-tls1_3",
          "-no_ign_eof",
          "-showcerts",
          "-CAfile",
          "/etc/mcp-host-probe/ca.pem",
          "-cert",
          "/etc/mcp-host-probe/client.crt",
          "-key",
          "/etc/mcp-host-probe/client.key",
        ],
        undefined,
        5000,
      );
      const serverCertificateSha256 = verifyNativeHostTls(output, expected);
      await put(
        join(
          this.root,
          `native-host-tls-${randomBytes(8).toString("hex")}.json`,
        ),
        JSON.stringify({ version: 1, serverCertificateSha256, verified: true }),
      );
    } finally {
      if (this.containers.has(role)) await this.remove(role);
    }
  }
  async verifyNetwork(probes: boolean) {
    check(this.guardIds.size === 3 && this.networkId, "guard_set");
    const results: string[] = [];
    await this.verifyNativeRoute();
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
      check(
        JSON.stringify(Object.keys(guard.NetworkSettings.Networks).sort()) ===
          JSON.stringify(
            [
              this.name("network"),
              ...(role === "coordinator" && this.inputs.backend === "vz"
                ? [this.name("native-network")]
                : []),
            ].sort(),
          ),
        "guard_network_membership",
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
        'for tool in /bin/sh /bin/bash /bin/sleep /bin/mkdir /bin/chown /bin/chmod /usr/bin/find /usr/bin/sha256sum /usr/bin/skopeo /usr/sbin/nft /usr/bin/dig /usr/bin/nc /usr/bin/curl /usr/bin/mksquashfs /usr/sbin/ip /usr/bin/openssl /usr/bin/stat /usr/bin/readlink /bin/cat; do test -x "$tool"; done',
      ]);
      const version = (
        await this.call(["exec", id, "/usr/bin/skopeo", "--version"])
      ).toString();
      check(/^skopeo version [0-9]+\./.test(version), "guard_skopeo");
      check(
        (await this.call(["exec", id, "/usr/bin/openssl", "version"]))
          .toString()
          .startsWith("OpenSSL 3."),
        "guard_openssl3",
      );
    } finally {
      await this.remove("tool-check");
    }
  }
  async nativeDns(
    resolve: (name: string, options: { all: true }) => Promise<LookupAddress[]> = lookup,
  ): Promise<void> {
    for (const [name, expected] of Object.entries(dnsRecords(this.inputs))) {
      let actual: LookupAddress[];
      try {
        actual = await resolve(name, { all: true });
      } catch {
        throw new PlatformFailure("NEEDS_CONTEXT", "native_dns_lookup");
      }
      if (!actual.length || actual.some((entry) => entry.address !== expected))
        throw new PlatformFailure("NEEDS_CONTEXT", "native_dns_route");
    }
  }
  async cleanup(): Promise<void> {
    if (this.fc && this.containers.has("host-agent")) {
      try {
        await this.fc.requireReleased();
      } catch (error) {
        await put(
          join(this.root, "fc-cleanup-blocked.json"),
          JSON.stringify({
            runId: this.runId,
            hostContainer: this.containers.get("host-agent"),
            action: "retained_for_identity_scoped_recovery",
            reason:
              error instanceof PlatformFailure
                ? error.seam
                : "nbd_state_unknown",
          }),
        );
        throw error;
      }
    }
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
    if (!this.nativeNetworkId && this.nativeNetworkName) {
      try {
        const ids = (
          await this.call([
            "network",
            "ls",
            "--no-trunc",
            "--filter",
            `label=${this.label()}`,
            "--filter",
            `name=${this.nativeNetworkName}`,
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
          "cleanup_native_network_identity",
        );
        this.nativeNetworkId = ids[0];
      } catch {
        failed = true;
      }
    }
    if (this.nativeNetworkId && this.containers.size === 0) {
      try {
        const [network] = JSON.parse(
          (
            await this.call(["network", "inspect", this.nativeNetworkId])
          ).toString(),
        );
        check(
          network?.Id === this.nativeNetworkId &&
            network.Name === this.nativeNetworkName &&
            network.Labels?.["engrams.mcp.run"] === this.runId &&
            Object.keys(network.Containers ?? {}).length === 0,
          "native_network_cleanup",
        );
        await this.call(["network", "rm", this.nativeNetworkId]);
        this.nativeNetworkId = undefined;
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
    if (this.fc) {
      try {
        await this.fc.requireStopped();
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
    if (survivors || this.networkId || this.nativeNetworkId) failed = true;
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
        networkRemaining: !!this.networkId || !!this.nativeNetworkId,
        success: !failed,
      }),
    );
    if (failed) throw new PlatformFailure("FAILED", "engine_cleanup");
  }
}
