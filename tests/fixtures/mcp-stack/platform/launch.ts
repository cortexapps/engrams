import { request as httpsRequest } from "node:https";
import { chmod, lstat, mkdir, open } from "node:fs/promises";
import { join, relative } from "node:path";
import { randomBytes } from "node:crypto";
import type {
  Context,
  ConfigurationId,
} from "../../../../orchestrator/scripts/mcp-stack/context.ts";
import {
  ROLES,
  type MutableRole,
  type Role,
  type SupervisorStatus,
} from "../../../../orchestrator/scripts/mcp-stack/supervisor.ts";
import {
  jsonRequest,
  tlsOptions,
} from "../../../../orchestrator/scripts/mcp-stack/transport.ts";
import { ZonePolicy } from "../../../../mcp-worker/src/network.ts";
import { FixtureObserver } from "../../../../orchestrator/scripts/mcp-stack/observer.ts";
import { RunnerFailure } from "../../../../orchestrator/scripts/mcp-stack/failure.ts";
import {
  check,
  childEnvironment,
  command,
  hash,
  OwnedProcess,
  PlatformFailure,
  put,
  readOwned,
  verifyPin,
  type Pin,
} from "./owned.ts";
import { address, dnsHosts, dnsRecords } from "./network.ts";
import { type Inputs, REPO } from "./preflight.ts";
import type { Identity } from "./identity.ts";
import { Engine } from "./engine.ts";
import type { stageAssets } from "./assets.ts";
import type { generateConfiguration } from "./configuration.ts";

type Assets = Awaited<ReturnType<typeof stageAssets>>;
type Configuration = Awaited<ReturnType<typeof generateConfiguration>>;
type NativeIdentity = {
  pid: number;
  uid: number;
  parent: number;
  group: number;
  seconds: string;
  microseconds: string;
  executable: string;
};
export function parseNativeIdentities(
  value: unknown,
  root: string,
): NativeIdentity[] {
  check(Array.isArray(value) && value.length < 4096, "native_identity_count");
  return value.map((v: unknown) => {
    check(
      v !== null && typeof v === "object" && !Array.isArray(v),
      "native_identity",
    );
    const p = v as Record<string, unknown>;
    check(
      Object.keys(p).sort().join(",") ===
        "executable,group,microseconds,parent,pid,seconds,uid",
      "native_identity_keys",
    );
    for (const key of ["pid", "uid", "parent", "group"])
      check(
        typeof p[key] === "number" &&
          Number.isSafeInteger(p[key]) &&
          Number(p[key]) >= 0,
        "native_identity_number",
      );
    check(
      Number(p.pid) > 1 &&
        p.uid === process.getuid?.() &&
        typeof p.seconds === "string" &&
        /^[0-9]{1,20}$/.test(p.seconds) &&
        typeof p.microseconds === "string" &&
        /^[0-9]{1,6}$/.test(p.microseconds) &&
        Number(p.microseconds) < 1000000,
      "native_identity_birth",
    );
    check(
      typeof p.executable === "string" &&
        (p.executable === join(root, "assets/hostAgent") ||
          p.executable === join(root, "assets/proxy") ||
          (p.executable.startsWith(join(root, "host") + "/") &&
            p.executable.endsWith("/netstack") &&
            !p.executable.split("/").includes(".."))),
      "native_identity_scope",
    );
    // Every field has an exact primitive check above.
    return p as NativeIdentity;
  });
}
const nativeKey = (p: NativeIdentity) =>
  JSON.stringify([p.pid, p.uid, p.seconds, p.microseconds, p.executable]);
export const hostListenAddress = (
  inputs: Pick<Inputs, "backend" | "nativeHostIp">,
) =>
  inputs.backend === "firecracker"
    ? "0.0.0.0:9101"
    : `${inputs.nativeHostIp}:9101`;
type RoleConfig = { env: Record<string, string>; files: Record<string, Pin> };
export class Stack {
  private host: OwnedProcess | undefined;
  private hostBoot = "not-started";
  private nativeIdentities = new Map<number, NativeIdentity>();
  private readonly stopped = new Set<Role>();
  private readonly active = new Map<
    Role,
    { id: string; configuration: ConfigurationId }
  >();
  private readonly configs = new Map<
    MutableRole,
    Partial<Record<ConfigurationId, RoleConfig>>
  >();
  private readonly projections = new Map<string, string>();
  private readonly environmentFiles = new Map<string, Pin>();
  constructor(
    readonly inputs: Inputs,
    readonly root: string,
    readonly identity: Identity,
    readonly assets: Assets,
    readonly configuration: Configuration,
    readonly engine: Engine,
  ) {}
  private secret(name: keyof Identity["secrets"]) {
    return readOwned(this.identity.secrets[name].path).then((bytes) =>
      bytes.toString(),
    );
  }
  private files(pins: Pin[]) {
    return Object.fromEntries(
      pins.map((pin) => [relative(this.root, pin.path), pin]),
    );
  }
  async prepare(): Promise<Context> {
    const work = join(this.root, "host");
    await mkdir(work, { mode: 0o700 });
    if (this.inputs.backend === "firecracker")
      await mkdir(join(this.root, "thin-nodes"), { mode: 0o700 });
    await put(join(work, "host_id"), this.identity.hostId);
    await put(join(work, "host.jsonl"), "");
    await put(join(work, "egress-proxyd.log"), "");
    await mkdir(join(work, "egress-ca"), { mode: 0o700 });
    await put(
      join(work, "egress-ca/ca.pem"),
      await readOwned(this.identity.ca.path),
    );
    await put(
      join(work, "egress-ca/ca.key"),
      await readOwned(this.identity.caKey.path),
    );
    const dbName = `mcp_${this.identity.runId.replaceAll("-", "_")}`;
    const pgPassword = await this.secret("postgres");
    const cDsn = await put(
      join(this.root, "coordinator.dsn"),
      `postgresql://fixture:${pgPassword}@postgres.fixture.example:5432/${dbName}`,
    );
    const oDsn = await put(
      join(this.root, "orchestrator.dsn"),
      `postgresql://fixture:${pgPassword}@postgres.fixture.example:5432/${dbName}`,
    );
    const client = this.identity.leaves;
    const c = this.configuration;
    const shared = {
      NODE_ENV: "production",
      SSL_CERT_FILE: this.identity.ca.path,
      ENGRAM_KEK_MASTER_KEY: await this.secret("kek"),
    };
    const baseO = {
      ...shared,
      ORCHESTRATOR_DATABASE_URL: (await readOwned(oDsn.path)).toString(),
      ORCHESTRATOR_PORT: "8787",
      ORCHESTRATOR_PUBLIC_URL: "https://app.fixture.example",
      CONTROL_PLANE_GRPC_URL: `http://${address(this.inputs, "coordinator")}:50061`,
      CONTROL_PLANE_HTTP_URL: "https://coordinator.fixture.example",
      NODE_EXTRA_CA_CERTS: this.identity.ca.path,
      CONTROL_PLANE_BEARER: await this.secret("appGrpc"),
      CONNECTION_BROKER_BEARER: await this.secret("broker"),
      BETTER_AUTH_SECRET: await this.secret("auth"),
      ENGRAM_DEPLOYMENT_ID: this.identity.runId,
      ORCHESTRATOR_AUTH_MODE: "password",
      ORCHESTRATOR_AUTH_PASSWORD_SIGNUP: "true",
      ORCHESTRATOR_ADMIN_EMAILS:
        "admin@fixture.example,bootstrap@fixture.example",
      TRUSTED_ORIGINS: "https://app.fixture.example",
      MCP_ENABLED: "true",
      MCP_RUNTIME_CONFIG: c.runtimeNormal.path,
    };
    const oFiles = this.files([
      this.identity.ca,
      client.orchestrator.certificate,
      client.orchestrator.key,
      c.runtimeNormal,
      c.runtimeZoneDisabled,
    ]);
    this.configs.set("orchestrator", {
      normal: { env: baseO, files: oFiles },
      "global-disabled": {
        env: { ...baseO, MCP_ENABLED: "false" },
        files: oFiles,
      },
      "zone-disabled": {
        env: { ...baseO, MCP_RUNTIME_CONFIG: c.runtimeZoneDisabled.path },
        files: oFiles,
      },
    });
    this.configs.set("coordinator", {
      normal: {
        env: {
          ...shared,
          DATABASE_URL: (await readOwned(cDsn.path)).toString(),
          ENGRAM_MODE: "coordinator",
          ENGRAM_BIND_ADDR: "0.0.0.0:8080",
          ENGRAM_APP_GRPC_ADDR: "0.0.0.0:50061",
          ENGRAM_HARNESS_LISTEN_ADDR: "0.0.0.0:50060",
          ENGRAM_AUTH_TOKENS: `${await this.secret("coordinator")},${await this.secret("appGrpc")}`,
          ENGRAM_APP_GRPC_TOKENS: await this.secret("appGrpc"),
          ENGRAM_HOST_CLIENT_TLS_CONFIG: c.coordinatorHostTls.path,
          ENGRAM_MCP_CONTROL_CONFIG: c.coordinator.path,
          ENGRAM_BLOB_BACKEND: "gcs",
          ENGRAM_GCS_BUCKET: this.identity.runId,
          STORAGE_EMULATOR_HOST: `http://${address(this.inputs, "blob")}:4443`,
          ENGRAM_LOCAL_PATH: "/tmp/coordinator",
          ENGRAM_LOG_FORMAT: "json",
          ENGRAM_CONNECTION_BROKER_BEARER: await this.secret("broker"),
          ENGRAM_ORCHESTRATOR_INTERNAL_URL: "https://app.fixture.example",
        },
        files: this.files([
          this.identity.ca,
          client.coordinator.certificate,
          client.coordinator.key,
          c.coordinator,
          c.coordinatorHostTls,
          c.clientIdentity,
          c.candidateFile,
        ]),
      },
    });
    this.configs.set("worker", {
      normal: {
        env: { NODE_ENV: "production", MCP_WORKER_ENABLED: "true" },
        files: {
          "config.json": c.worker,
          "tls/tls.crt": client.worker.certificate,
          "tls/tls.key": client.worker.key,
          "tls/ca.crt": this.identity.ca,
        },
      },
    });
    this.configs.set("host-agent", {
      normal: {
        env: {
          ENGRAM_SANDBOX_BACKEND: this.inputs.backend,
          ENGRAM_SANDBOX_WORK_DIR: work,
          ENGRAM_DIRTY_ROOT: join(work, "dirty"),
          ENGRAM_COORDINATOR_ENDPOINT:
            "http://" + address(this.inputs, "coordinator") + ":8080",
          ENGRAM_COORDINATOR_TOKEN: await this.secret("coordinator"),
          ENGRAM_GRPC_LISTEN_ADDR: hostListenAddress(this.inputs),
          ENGRAM_GRPC_ADVERTISE_ADDR: "https://host.fixture.example:9101",
          ENGRAM_HOST_MANAGED_TLS_CONFIG: c.hostTls.path,
          ENGRAM_BUNDLE_DIR: this.assets.bundles,
          ENGRAM_HOST_METRICS_ADDR: "127.0.0.1:0",
          ENGRAM_MIGRATE_PEER_LISTEN_ADDR: "disabled",
          ENGRAM_BLOB_BACKEND: "gcs",
          ENGRAM_GCS_BUCKET: this.identity.runId,
          STORAGE_EMULATOR_HOST: `http://${address(this.inputs, "blob")}:4443`,
          ENGRAM_LOG_FORMAT: "json",
          ENGRAM_EGRESS_CA_SOURCE: "local-disk",
          ENGRAM_EGRESS_CA_DIR: join(work, "egress-ca"),
          ENGRAM_EGRESS_PROXYD_BIN: this.assets.proxy.path,
          ENGRAM_EGRESS_PROXY_PORT: String(this.inputs.registryPort + 1),
          ENGRAM_EGRESS_DNS_PORT: String(this.inputs.registryPort + 2),
          ENGRAM_GUEST_GATEWAY_PORT: String(this.inputs.registryPort + 3),
          ...(this.inputs.backend === "vz"
            ? {
                ENGRAM_VZ_KERNEL_PATH: this.assets.binaries.kernel.path,
                ENGRAM_MANAGED_NETSTACK_PATH: this.assets.binaries.helper.path,
                ENGRAM_MANAGED_NETSTACK_SHA256:
                  this.assets.binaries.helper.sha256,
              }
            : {
                ENGRAM_KERNEL_IMAGE_PATH: this.assets.binaries.kernel.path,
                ENGRAM_FIRECRACKER_BIN: "/usr/local/bin/firecracker",
                ENGRAM_FC_UFFD_HANDLER_BIN:
                  "/usr/local/bin/engram-uffd-handler",
                ...this.engine.fc?.environment(),
                MCP_FIXTURE_HOST_LOG: join(work, "host.jsonl"),
              }),
        },
        files: this.files([
          this.identity.ca,
          client.host.certificate,
          client.host.key,
          c.hostTls,
        ]),
      },
    });
    const configurations: Context["supervisor"]["configurations"] = {
      orchestrator: { normal: c.runtimeNormal },
      coordinator: { normal: c.coordinator },
      worker: { normal: c.worker },
      "host-agent": { normal: c.hostTls },
    };
    for (const [role, variants] of this.configs) {
      for (const [variant, config] of Object.entries(variants)) {
        const pin = await put(
          join(this.root, `${role}-${variant}.json`),
          JSON.stringify(config),
        );
        if (role === "orchestrator") {
          check(
            variant === "normal" ||
              variant === "global-disabled" ||
              variant === "zone-disabled",
            "role_variant",
          );
          configurations.orchestrator[variant] = pin;
        } else {
          check(variant === "normal", "role_variant");
          configurations[role].normal = pin;
        }
        if (role !== "host-agent") {
          const env =
            Object.entries(config.env)
              .map(([key, value]) => {
                check(!/[\r\n]/.test(value), "environment_line");
                return `${key}=${value}`;
              })
              .join("\n") + "\n";
          this.environmentFiles.set(
            `${role}-${variant}`,
            await put(join(this.root, `${role}-${variant}.env`), env),
          );
        }
      }
    }
    const fixture = async (kind: "modern" | "legacy") => {
      const runRoot = join(this.root, kind);
      await mkdir(runRoot, { mode: 0o700 });
      const copy = async (name: string, pin: Pin) =>
        put(join(runRoot, name), await readOwned(pin.path));
      const certificate = await copy("server.pem", client[kind].certificate),
        key = await copy("server.key", client[kind].key),
        ca = await copy("ca.pem", this.identity.ca);
      const token = await put(
        join(runRoot, "observer.token"),
        randomBytes(32).toString("base64url"),
      );
      const resource = await put(
        join(runRoot, "resource.key"),
        randomBytes(32).toString("base64url"),
      );
      const origin = (role: string) =>
        `https://${role}.${kind}.fixture.example:8443`;
      const protocol =
        kind === "modern" ? ("2026-07-28" as const) : ("2025-06-18" as const);
      const configuration = await put(
        join(runRoot, "config.json"),
        JSON.stringify({
          version: 1,
          runRoot,
          listen: { host: address(this.inputs, kind), port: 8443 },
          advertised: {
            resource: origin("resource"),
            issuer: origin("issuer"),
            authorization: origin("authorization"),
            token: origin("token"),
            redirectUri:
              "https://app.fixture.example/integrations/mcp/callback",
            authorities: ["resource", "issuer", "authorization", "token"].map(
              (role) => `${role}.${kind}.fixture.example:8443`,
            ),
          },
          tls: {
            certificateFile: certificate.path,
            keyFile: key.path,
            caFile: ca.path,
          },
          control: {
            host: address(this.inputs, kind),
            port: 9444,
            origin: `https://observer.${kind}.fixture.example:9444`,
            tokenFile: token.path,
          },
          protocol,
          case: { requireAuth: true },
          apiKeyFile: resource.path,
        }),
      );
      return {
        origin: `https://observer.${kind}.fixture.example:9444`,
        ca,
        serverFingerprint: client[kind].fingerprint,
        configuration,
        runRoot,
        tokenFile: token.path,
        resourceKeyFile: resource.path,
        resourceOrigin: origin("resource"),
        issuerOrigin: origin("issuer"),
        authorizationOrigin: origin("authorization"),
        tokenOrigin: origin("token"),
        redirectUri: "https://app.fixture.example/integrations/mcp/callback",
        protocol,
      };
    };
    const modern = await fixture("modern"),
      legacy = await fixture("legacy");
    const endpoint = (origin: string, leaf: keyof Identity["leaves"]) => ({
      origin,
      ca: this.identity.ca,
      serverFingerprint: client[leaf].fingerprint,
    });
    const human = (role: "admin" | "owner" | "other") => ({
      email: `${role}@fixture.example`,
      passwordFile: this.identity.secrets[`${role}Password`].path,
      cookieFile: join(this.root, `${role}.cookie`),
    });
    return {
      version: 1,
      runId: this.identity.runId,
      runRoot: this.root,
      backend: this.inputs.backend,
      sourceCommit: this.inputs.sourceCommit,
      hostId: this.identity.hostId,
      // These fields are filled from the actual public enable result before the context is written or handed to the runner.
      imageId: "pending-enable",
      imageDigest: `sha256:${"0".repeat(64)}`,
      artifacts: this.assets.artifacts,
      endpoints: {
        app: endpoint("https://app.fixture.example", "app"),
        coordinator: endpoint(
          "https://coordinator.fixture.example",
          "coordinator",
        ),
        operator: {
          ...endpoint("https://operator.fixture.example:8445", "coordinator"),
          certificate: client.operator.certificate,
          privateKeyFile: client.operator.key.path,
        },
        workerControl: endpoint(
          "https://worker.fixture.example:8443",
          "worker",
        ),
      },
      protocolFixtures: {
        modern: { ...modern, protocol: "2026-07-28" },
        legacy: { ...legacy, protocol: "2025-06-18" },
      },
      publication: {
        candidateIdentity: c.candidateIdentity,
        expectedGeneration: null,
        deploymentIdentity: c.infrastructure.deploymentIdentity,
        networkIdentity: c.infrastructure.networkIdentity,
        trustIdentity: c.infrastructure.trustIdentity,
        zone: c.zone.id,
      },
      identities: {
        admin: human("admin"),
        owner: human("owner"),
        other: human("other"),
        unattendedKeyFile: join(this.root, "unattended.key"),
        automationKeyFile: join(this.root, "automation.key"),
        routerSentinelFile: this.identity.secrets.router.path,
      },
      supervisor: {
        socket: join(this.root, "supervisor.sock"),
        tokenFile: this.identity.secrets.supervisor.path,
        configurations,
        operatorCandidates: {
          normal: { file: c.candidateFile, identity: c.candidateIdentity },
        },
      },
      observer: {
        orchestratorDsnFile: oDsn.path,
        coordinatorDsnFile: cDsn.path,
        hostManagedRoot: join(work, "managed-boots"),
        hostLogFile: join(work, "egress-proxyd.log"),
      },
      guest: {
        stateDirectory: "/tmp/mcp-stack-fixture",
        cliPath: "/opt/engram/dyn/0/engram-mcp-model-fixture",
      },
      deadlines: { requestMs: 30000, stepMs: 120000, totalMs: 3600000 },
      evidenceFile: join(this.root, "evidence.jsonl"),
    };
  }
  async dependencies() {
    const e = this.engine;
    const database = `mcp_${this.identity.runId.replaceAll("-", "_")}`;
    const env = await put(
      join(this.root, "postgres.env"),
      `POSTGRES_USER=fixture\nPOSTGRES_PASSWORD=${await this.secret("postgres")}\nPOSTGRES_DB=${database}\nPGDATA=/var/lib/postgresql/data\n`,
    );
    const data = await e.project("postgres-data", {}, 999);
    const pg = await e.create("postgres", this.inputs.images.postgres.id, [
      ...e.networkOptions("postgres"),
      "--user",
      "999:999",
      "--read-only",
      "--env-file",
      env.path,
      "--mount",
      `type=volume,src=${data},dst=/var/lib/postgresql/data`,
      "--tmpfs",
      "/var/run/postgresql:rw,nosuid,nodev,size=8m,mode=1777",
      "--tmpfs",
      "/tmp:rw,nosuid,nodev,size=8m,mode=1777",
    ]);
    await this.poll(async () => {
      await e.call([
        "exec",
        pg,
        "/usr/bin/pg_isready",
        "-U",
        "fixture",
        "-d",
        database,
      ]);
    });
    const blob = await e.project("blob-data", {});
    await e.create(
      "blob",
      this.inputs.images.blob.id,
      [
        ...e.networkOptions("blob"),
        "--user",
        "1000:1000",
        "--read-only",
        "--mount",
        `type=volume,src=${blob},dst=/data`,
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=16m,mode=1777",
      ],
      [
        "-scheme",
        "http",
        "-port",
        "4443",
        "-filesystem-root",
        "/data",
        "-public-host",
        `${address(this.inputs, "blob")}:4443`,
      ],
      "/bin/fake-gcs-server",
    );
    const core = await put(
      join(this.root, "Corefile"),
      `.:53 {\n hosts /projection/hosts {\n ttl 1\n }\n }\n`,
    );
    const hosts = await put(
      join(this.root, "dns-hosts"),
      dnsHosts(this.inputs),
    );
    const dns = await e.project("dns-config", { Corefile: core, hosts });
    await e.create(
      "dns",
      this.inputs.images.dns.id,
      [
        ...e.networkOptions("dns"),
        "--cap-add",
        "NET_BIND_SERVICE",
        "--user",
        "1000:1000",
        "--read-only",
        "--mount",
        `type=volume,src=${dns},dst=/projection,readonly`,
      ],
      ["-conf", "/projection/Corefile"],
      "/coredns",
    );
    const sink = await e.create(
      "denied",
      this.inputs.images.guard.id,
      [
        ...e.networkOptions("denied"),
        "--user",
        "1000:1000",
        "--read-only",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=4m",
      ],
      ["-l", "-k", "8080"],
      "/usr/bin/nc",
    );
    const stub = await put(
      join(this.root, "denied-Corefile"),
      `.:53 {\n template IN A {\n answer "{{ .Name }} 60 IN A ${address(this.inputs, "denied")}"\n }\n }\n`,
    );
    const stubVolume = await e.project("denied-dns-config", { Corefile: stub });
    await e.create(
      "denied-dns",
      this.inputs.images.dns.id,
      [
        "--network",
        `container:${sink}`,
        "--cap-add",
        "NET_BIND_SERVICE",
        "--user",
        "1000:1000",
        "--read-only",
        "--mount",
        `type=volume,src=${stubVolume},dst=/projection,readonly`,
      ],
      ["-conf", "/projection/Corefile"],
      "/coredns",
    );
    const probe = await e.create(
      "dependency-probe",
      this.inputs.images.guard.id,
      [
        "--network",
        this.engine.networkName(),
        "--user",
        "1000:1000",
        "--read-only",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=4m",
      ],
      ["infinity"],
      "/bin/sleep",
    );
    await this.poll(async () => {
      await e.call([
        "exec",
        probe,
        "/usr/bin/nc",
        "-z",
        "-w",
        "1",
        address(this.inputs, "denied"),
        "8080",
      ]);
    });
    const response = await e.call([
      "exec",
      probe,
      "/usr/bin/dig",
      "+short",
      `@${address(this.inputs, "denied")}`,
      "unrouted.invalid",
      "A",
    ]);
    check(
      response.toString().trim() === address(this.inputs, "denied"),
      "owned_external_stub_live",
    );
    await this.poll(async () => {
      await e.call([
        "exec",
        probe,
        "/usr/bin/curl",
        "--fail",
        "--silent",
        "--max-time",
        "2",
        `http://${address(this.inputs, "blob")}:4443/storage/v1/b`,
      ]);
    });
    await e.call([
      "exec",
      probe,
      "/usr/bin/curl",
      "--fail",
      "--silent",
      "--max-time",
      "2",
      "-X",
      "POST",
      "-H",
      "Content-Type: application/json",
      "--data",
      JSON.stringify({ name: this.identity.runId }),
      `http://${address(this.inputs, "blob")}:4443/storage/v1/b?project=fixture`,
    ]);
    await e.remove("dependency-probe");
  }
  private async poll(action: () => Promise<void>) {
    const deadline = performance.now() + 30000;
    while (true) {
      this.engine.checkCancellation();
      try {
        await action();
        return;
      } catch {
        if (performance.now() >= deadline)
          throw new PlatformFailure("FAILED", "dependency_readiness");
        await new Promise((r) => setTimeout(r, 100));
      }
    }
  }
  async edges() {
    for (const role of ["orchestrator", "coordinator"] as const) {
      const leaf =
        this.identity.leaves[role === "orchestrator" ? "app" : "coordinator"];
      const port = role === "orchestrator" ? 8787 : 8080;
      const config = await put(
        join(this.root, `${role}-edge.conf`),
        `pid /tmp/nginx.pid;\nerror_log /dev/stderr error;\nevents {}\nhttp { access_log off; client_body_temp_path /tmp/client; proxy_temp_path /tmp/proxy; fastcgi_temp_path /tmp/fastcgi; uwsgi_temp_path /tmp/uwsgi; scgi_temp_path /tmp/scgi; server { listen 443 ssl; ssl_certificate /projection/tls.crt; ssl_certificate_key /projection/tls.key; ssl_protocols TLSv1.3; location / { proxy_pass http://127.0.0.1:${port}; proxy_http_version 1.1; proxy_set_header Host $host; proxy_set_header X-Forwarded-Proto https; proxy_set_header Upgrade $http_upgrade; proxy_set_header Connection upgrade; } } }\n`,
      );
      const volume = await this.engine.project(`${role}-edge`, {
        "edge.conf": config,
        "tls.crt": leaf.certificate,
        "tls.key": leaf.key,
      });
      await this.engine.create(
        `${role}-edge`,
        this.inputs.images.edge.id,
        [
          "--network",
          `container:${this.engine.guardIds.get(role)}`,
          "--user",
          "1000:1000",
          "--read-only",
          "--mount",
          `type=volume,src=${volume},dst=/projection,readonly`,
          "--tmpfs",
          "/tmp:rw,noexec,nosuid,nodev,size=16m,mode=1777",
        ],
        ["-c", "/projection/edge.conf", "-g", "daemon off;"],
        "/usr/sbin/nginx",
      );
    }
    const data = await this.engine.project("registry-data", {});
    const registryEnv = await put(
      join(this.root, "registry.env"),
      `REGISTRY_HTTP_ADDR=0.0.0.0:${this.inputs.registryPort}\nREGISTRY_STORAGE_FILESYSTEM_ROOTDIRECTORY=/var/lib/registry\n`,
    );
    await this.engine.create("registry", this.inputs.images.registry.id, [
      "--network",
      `container:${this.engine.guardIds.get("coordinator")}`,
      "--user",
      "1000:1000",
      "--read-only",
      "--env-file",
      registryEnv.path,
      "--mount",
      `type=volume,src=${data},dst=/var/lib/registry`,
      "--tmpfs",
      "/tmp:rw,nosuid,nodev,size=4m",
    ]);
  }
  async startRole(
    ctx: Context,
    role: MutableRole,
    configuration: ConfigurationId,
  ): Promise<void> {
    const config = this.configs.get(role)?.[configuration];
    check(config && !this.active.has(role), "role_start");
    check(
      role === "orchestrator" || configuration === "normal",
      "role_variant",
    );
    const configPin =
      role === "orchestrator"
        ? ctx.supervisor.configurations.orchestrator[configuration]
        : ctx.supervisor.configurations[role].normal;
    check(configPin, "role_configuration");
    await verifyPin(configPin);
    for (const file of Object.values(config.files)) await verifyPin(file);
    if (role === "host-agent") {
      if (this.inputs.backend === "firecracker") {
        check(this.engine.fc, "fc_inventory_missing");
        const image = this.inputs.images.hostAgent;
        check(image, "fc_host_image");
        const env = config.env;
        const envFile = await put(
          join(this.root, `host-env-${randomBytes(6).toString("hex")}`),
          Object.entries(env)
            .map(([k, v]) => {
              check(!/[\r\n]/.test(v), "host_environment_line");
              return `${k}=${v}`;
            })
            .join("\n") + "\n",
        );
        const id = await this.engine.create("host-agent", image.id, [
          "--network",
          `container:${this.engine.guardIds.get("coordinator")}`,
          "--pid",
          "host",
          "--cgroupns",
          "private",
          "--user",
          "0:0",
          "--read-only",
          "--cap-add",
          "NET_ADMIN",
          "--cap-add",
          "SYS_ADMIN",
          "--device",
          "/dev/kvm:/dev/kvm:rw",
          "--device",
          "/dev/net/tun:/dev/net/tun:rw",
          ...this.engine.fc.deviceOptions(),
          "--env-file",
          envFile.path,
          ...["host", "assets", "bundles", "tls", "configuration"].flatMap(
            (directory) => [
              "--mount",
              `type=bind,src=${join(this.root, directory)},dst=${join(this.root, directory)}${directory === "host" ? "" : ",readonly"}`,
            ],
          ),
          "--tmpfs",
          "/run:rw,nosuid,nodev,size=64m",
          "--tmpfs",
          "/tmp:rw,nosuid,nodev,size=128m,mode=1777",
        ]);
        this.hostBoot = id;
        this.active.set(role, { id, configuration });
        return;
      }
      check(
        (await this.nativeProcesses()).length === 0,
        "native_processes_before_start",
      );
      const hostLog = join(this.root, "host", "host.jsonl");
      const log = await open(hostLog, "a", 0o600);
      await chmod(hostLog, 0o600);
      this.engine.checkCancellation();
      try {
        this.host = new OwnedProcess(
          this.assets.binaries.hostAgent.path,
          [],
          this.root,
          { ...childEnvironment(this.root), ...config.env },
          ["ignore", log.fd, log.fd],
        );
      } finally {
        await log.close();
      }
      await this.poll(async () => {
        const found = (await this.nativeProcesses()).find(
          (p) => p.pid === this.host?.child.pid,
        );
        check(found, "native_host_identity");
      });
      const live = this.nativeIdentities.get(this.host.child.pid!);
      check(live, "native_host_identity");
      this.hostBoot = `${live.pid}-${live.seconds}-${live.microseconds}`;
      this.active.set(role, { id: this.hostBoot, configuration });
      return;
    }
    const key = `${role}-${configuration}`;
    let volume = this.projections.get(key);
    if (!volume) {
      volume = await this.engine.project(key, config.files);
      this.projections.set(key, volume);
    }
    const guard = this.engine.guardIds.get(role);
    check(guard, "role_guard");
    const envFile = this.environmentFiles.get(key);
    check(envFile, "role_environment");
    await verifyPin(envFile);
    const id = await this.engine.create(role, this.inputs.images[role].id, [
      "--network",
      `container:${guard}`,
      "--user",
      "1000:1000",
      "--read-only",
      "--env-file",
      envFile.path,
      "--mount",
      `type=volume,src=${volume},dst=${role === "worker" ? "/etc/mcp-worker" : this.root},readonly`,
      "--tmpfs",
      "/tmp:rw,noexec,nosuid,nodev,size=128m,mode=1777",
    ]);
    this.active.set(role, { id, configuration });
  }
  async migrateOrchestrator(): Promise<void> {
    const config = this.configs.get("orchestrator")?.normal;
    check(config, "migration_configuration");
    const volume = await this.engine.project(
      "orchestrator-migration",
      config.files,
    );
    const env = this.environmentFiles.get("orchestrator-normal");
    check(env, "migration_environment");
    await verifyPin(env);
    const guard = this.engine.guardIds.get("orchestrator");
    check(guard, "migration_guard");
    const id = await this.engine.create(
      "orchestrator-migrate",
      this.inputs.images.orchestrator.id,
      [
        "--network",
        `container:${guard}`,
        "--user",
        "1000:1000",
        "--read-only",
        "--env-file",
        env.path,
        "--mount",
        `type=volume,src=${volume},dst=${this.root},readonly`,
        "--tmpfs",
        "/tmp:rw,noexec,nosuid,nodev,size=128m,mode=1777",
      ],
      ["run", "migrate"],
      "/usr/local/bin/bun",
    );
    check(
      (await this.engine.call(["wait", id], undefined, 30000))
        .toString()
        .trim() === "0",
      "orchestrator_migration",
    );
    await this.engine.remove("orchestrator-migrate");
  }
  async stop(role: MutableRole): Promise<void> {
    if (role === "host-agent") {
      await this.cleanupHost(false);
    } else await this.engine.remove(role);
    this.stopped.add(role);
    // Keep the last immutable identity for status of the stopped role.
  }
  async restart(
    ctx: Context,
    role: MutableRole,
    configuration: ConfigurationId,
  ): Promise<void> {
    await this.stop(role);
    this.active.delete(role);
    this.stopped.delete(role);
    await this.startRole(ctx, role, configuration);
    await this.readyRole(ctx, role);
  }
  private nativeHostTls() {
    return {
      ca: this.identity.ca,
      clientCertificate: this.identity.leaves.coordinator.certificate,
      clientKey: this.identity.leaves.coordinator.key,
      serverCertificate: this.identity.leaves.host.certificate,
    };
  }
  async readyRole(ctx: Context, role: MutableRole): Promise<void> {
    const deadline = performance.now() + 15000;
    while (performance.now() < deadline) {
      this.engine.checkCancellation();
      try {
        if (role === "orchestrator") {
          const res = await jsonRequest(
            ctx,
            ctx.endpoints.app,
            "GET",
            "/healthz",
          );
          check(res.status === 200, "orchestrator_readiness");
        } else if (role === "host-agent") {
          check(
            this.inputs.backend === "firecracker"
              ? (await this.engine.audit(this.hostBoot)).State.Running
              : this.host?.running(),
            "host_running",
          );
          const stat = await lstat(ctx.observer.hostManagedRoot);
          check(
            stat.isDirectory() && (stat.mode & 0o777) === 0o700,
            "host_managed_root",
          );
        } else {
          if (role === "worker") {
            const tls = await tlsOptions(ctx, ctx.endpoints.workerControl);
            const certificate = await readOwned(
              this.identity.leaves.coordinator.certificate.path,
            );
            const key = await readOwned(
              this.identity.leaves.coordinator.key.path,
            );
            const identity = await new Promise<unknown>((resolve, reject) => {
              const request = httpsRequest(
                new URL("/identity", ctx.endpoints.workerControl.origin),
                { ...tls, cert: certificate, key, method: "GET", agent: false },
                (response) => {
                  const chunks: Buffer[] = [];
                  let bytes = 0;
                  response.on("data", (chunk: Buffer) => {
                    bytes += chunk.length;
                    if (bytes > 4096) request.destroy();
                    else chunks.push(chunk);
                  });
                  response.once("error", reject);
                  response.once("end", () => {
                    clearTimeout(timer);
                    try {
                      check(
                        response.statusCode === 200,
                        "worker_readiness_status",
                      );
                      resolve(JSON.parse(Buffer.concat(chunks).toString()));
                    } catch (error) {
                      reject(error);
                    }
                  });
                },
              );
              const timer = setTimeout(() => request.destroy(), 2000);
              request.once("error", () => {
                clearTimeout(timer);
                reject(new PlatformFailure("FAILED", "worker_readiness_tls"));
              });
              request.end();
            });
            check(
              identity !== null &&
                typeof identity === "object" &&
                "bootId" in identity &&
                typeof identity.bootId === "string" &&
                "zoneId" in identity &&
                identity.zoneId === this.configuration.zone.id &&
                "policyDigest" in identity &&
                identity.policyDigest ===
                  new ZonePolicy(this.configuration.zone).digest,
              "worker_readiness_identity",
            );
          }
          if (role === "coordinator") {
            const response = await jsonRequest(
              ctx,
              ctx.endpoints.coordinator,
              "GET",
              "/readyz",
            );
            check(response.status === 200, "coordinator_readiness");
          }
          const active = this.active.get(role);
          check(active, "role_handle");
          const actual = await this.engine.inspect(active.id);
          check(actual.State.Running === true, "role_running");
        }
        if (role === "host-agent") {
          if (this.inputs.backend === "firecracker") {
            await this.engine.qualifyFcHost(this.hostBoot);
            check(this.engine.fc, "fc_inventory_missing");
            await this.engine.fc.adoptHost(
              (await this.engine.inspect(this.hostBoot)).State.Pid,
            );
            await this.engine.fc.observe();
          }
          await this.engine.qualifyNativeHost(this.nativeHostTls());
        }
        return;
      } catch {
        await new Promise((r) => setTimeout(r, 100));
      }
    }
    throw new PlatformFailure("FAILED", "role_readiness");
  }
  async status(ctx: Context): Promise<SupervisorStatus> {
    const states: SupervisorStatus["roles"] = [];
    for (const role of ROLES) {
      const active = this.active.get(role);
      check(active, "role_identity");
      const artifact =
        role === "host-agent"
          ? ctx.artifacts.hostAgent
          : role.startsWith("fixture-")
            ? ctx.artifacts.fixture
            : ctx.artifacts[role as "worker" | "orchestrator" | "coordinator"];
      await verifyPin(artifact);
      const config =
        role === "fixture-modern" || role === "fixture-legacy"
          ? ctx.protocolFixtures[
              role === "fixture-modern" ? "modern" : "legacy"
            ].configuration
          : role === "orchestrator"
            ? ctx.supervisor.configurations.orchestrator[active.configuration]
            : ctx.supervisor.configurations[role].normal;
      check(config, "role_configuration");
      await verifyPin(config);
      if (
        role === "orchestrator" ||
        role === "coordinator" ||
        role === "worker" ||
        role === "host-agent"
      ) {
        const current = this.configs.get(role)?.[active.configuration];
        check(current, "role_configuration");
        for (const file of Object.values(current.files)) await verifyPin(file);
        const envFile = this.environmentFiles.get(
          `${role}-${active.configuration}`,
        );
        if (envFile) await verifyPin(envFile);
      }
      let running = false;
      if (role === "host-agent") {
        if (this.inputs.backend === "firecracker") {
          if (!this.stopped.has(role)) {
            running =
              (await this.engine.audit(active.id)).State.Running === true;
            await this.engine.verifyResolver(active.id);
          }
        } else {
          const native = await this.nativeProcesses();
          running =
            !!this.host?.running() &&
            native.some((p) => p.pid === this.host?.child.pid);
        }
      } else if (!this.stopped.has(role)) {
        try {
          const actual = await this.engine.audit(active.id);
          await this.engine.verifyResolver(active.id);
          running = actual.State.Running === true;
          check(
            actual.Image ===
              this.inputs.images[
                role.startsWith("fixture-")
                  ? "fixture"
                  : (role as "worker" | "orchestrator" | "coordinator")
              ].id,
            "running_artifact",
          );
        } catch (error) {
          throw error;
        }
      }
      if (role === "host-agent" && running) {
        if (this.inputs.backend === "firecracker") {
          await this.engine.qualifyFcHost(active.id);
          check(this.engine.fc, "fc_inventory_missing");
          await this.engine.fc.qualifyKernel(
            (await this.engine.inspect(active.id)).State.Pid,
          );
          await this.engine.fc.observe();
        }
        await this.engine.qualifyNativeHost(this.nativeHostTls());
      }
      states.push({
        role,
        state: running ? "running" : "stopped",
        bootId: active.id,
        artifactSha256: artifact.sha256,
        configurationId: active.configuration,
        configurationSha256: config.sha256,
      });
    }
    return {
      version: 1,
      runId: ctx.runId,
      backend: ctx.backend,
      sourceCommit: ctx.sourceCommit,
      hostId: ctx.hostId,
      roles: states,
      network: await this.engine.verifyNetwork(true),
    };
  }
  async fixtures(ctx: Context): Promise<void> {
    for (const kind of ["modern", "legacy"] as const) {
      const fixture = ctx.protocolFixtures[kind];
      const role = `fixture-${kind}` as const;
      const id = await this.engine.create(
        role,
        this.inputs.images.fixture.id,
        [
          ...this.engine.networkOptions(kind),
          "--read-only",
          "--user",
          `${process.getuid?.()}:${process.getgid?.()}`,
          "--mount",
          `type=bind,src=${fixture.runRoot},dst=${fixture.runRoot}`,
          "--tmpfs",
          "/tmp:rw,noexec,nosuid,nodev,size=16m",
        ],
        ["/app/tests/fixtures/mcp-stack/main.mts", fixture.configuration.path],
        "/usr/local/bin/node",
      );
      this.active.set(role, { id, configuration: "normal" });
    }
    await this.engine.nativeDns();
    for (const kind of ["modern", "legacy"] as const) {
      try {
        await new FixtureObserver(ctx, kind).ready();
      } catch (error) {
        if (error instanceof RunnerFailure || error instanceof PlatformFailure)
          throw error;
        throw new PlatformFailure(
          "FAILED",
          kind === "modern"
            ? "fixture_modern_readiness"
            : "fixture_legacy_readiness",
        );
      }
    }
  }
  private async nativeProcesses(): Promise<NativeIdentity[]> {
    check(this.assets.nativeProbe, "native_probe");
    await verifyPin(this.assets.nativeProbe);
    const bytes = await command(
      this.assets.nativeProbe.path,
      [this.root],
      this.root,
      childEnvironment(this.root),
    );
    const found = parseNativeIdentities(
      JSON.parse(bytes.toString()),
      this.root,
    );
    for (const p of found) {
      const expected =
        p.executable === this.assets.binaries.hostAgent.path
          ? this.assets.binaries.hostAgent
          : p.executable === this.assets.proxy.path
            ? this.assets.proxy
            : {
                path: p.executable,
                sha256: this.assets.binaries.helper.sha256,
              };
      await verifyPin(expected);
      const previous = this.nativeIdentities.get(p.pid);
      if (previous)
        check(nativeKey(previous) === nativeKey(p), "native_pid_reused");
      this.nativeIdentities.set(p.pid, p);
    }
    return found;
  }
  async cleanupHost(requireRelease = true) {
    if (this.inputs.backend === "firecracker") {
      check(this.engine.fc, "fc_inventory_missing");
      if (requireRelease) await this.engine.fc.requireReleased();
      if (this.active.has("host-agent") && !this.stopped.has("host-agent"))
        await this.engine.remove("host-agent");
      if (requireRelease) await this.engine.fc.requireStopped();
      return;
    }
    const host = this.host;
    const leader = host && this.nativeIdentities.get(host.child.pid!);
    const before = await this.nativeProcesses();
    await put(
      join(this.root, `native-stop-${randomBytes(8).toString("hex")}.json`),
      JSON.stringify({ version: 1, identities: before }),
    );
    check(
      host ||
        !before.some(
          (p) => p.executable === this.assets.binaries.hostAgent.path,
        ),
      "native_host_handle_missing",
    );
    if (host?.running())
      check(
        leader &&
          leader.executable === this.assets.binaries.hostAgent.path &&
          before.some((p) => nativeKey(p) === nativeKey(leader)),
        "native_host_identity_before_stop",
      );
    const signalIdentity = async (
      identity: NativeIdentity,
      signal: NodeJS.Signals,
    ) => {
      // Compare the kernel birth time and executable again immediately before signaling.
      const current = (await this.nativeProcesses()).find(
        (p) => p.pid === identity.pid,
      );
      if (!current) return;
      check(
        nativeKey(identity) === nativeKey(current),
        "native_identity_changed",
      );
      try {
        process.kill(identity.pid, signal);
      } catch (error) {
        if (
          !(error instanceof Error && "code" in error && error.code === "ESRCH")
        )
          throw error;
      }
    };
    if (host) {
      let closed = false;
      void host.done.then(() => {
        closed = true;
      });
      const leaderPresent = async () =>
        leader &&
        (await this.nativeProcesses()).some(
          (p) => nativeKey(p) === nativeKey(leader),
        );
      // The host restarts its proxy. Stop and reap the leader before selecting children.
      for (const signal of ["SIGTERM", "SIGKILL"] as const) {
        if (leader) await signalIdentity(leader, signal);
        const until = performance.now() + 1500;
        while (performance.now() < until) {
          const present = await leaderPresent();
          if (closed && !host.running() && !present) break;
          await new Promise((r) => setTimeout(r, 20));
        }
        if (closed && !host.running() && !(await leaderPresent())) break;
      }
      check(closed && !host.running(), "native_host_handle_cleanup");
      check(!(await leaderPresent()), "native_host_survivor");
      this.host = undefined;
    }
    for (const signal of ["SIGTERM", "SIGKILL"] as const) {
      const children = await this.nativeProcesses();
      check(
        children.every(
          (p) => p.executable !== this.assets.binaries.hostAgent.path,
        ),
        "native_host_survivor",
      );
      for (const identity of children) await signalIdentity(identity, signal);
      const until = performance.now() + 1500;
      while ((await this.nativeProcesses()).length && performance.now() < until)
        await new Promise((r) => setTimeout(r, 20));
    }
    check((await this.nativeProcesses()).length === 0, "native_survivors");
    this.nativeIdentities.clear();
  }
}
