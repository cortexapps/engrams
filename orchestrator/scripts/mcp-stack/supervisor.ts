import { request } from "node:http";
import { z } from "zod";
import {
  privateFile,
  requireThat,
  RunnerFailure,
  type Context,
  type ConfigurationId,
} from "./context.ts";

export const ROLES = [
  "orchestrator",
  "worker",
  "coordinator",
  "host-agent",
  "fixture-modern",
  "fixture-legacy",
] as const;
export type Role = (typeof ROLES)[number];
export type MutableRole = Exclude<Role, "fixture-modern" | "fixture-legacy">;
const hash = z.string().regex(/^[a-f0-9]{64}$/);
const roleState = z.strictObject({
  role: z.enum(ROLES),
  state: z.enum(["running", "stopped"]),
  bootId: z.string().regex(/^[A-Za-z0-9_-]{1,96}$/),
  artifactSha256: hash,
  configurationId: z.enum(["normal", "global-disabled", "zone-disabled"]),
  configurationSha256: hash,
});
export const StatusSchema = z.strictObject({
  version: z.literal(1),
  runId: z.string(),
  backend: z.enum(["firecracker", "vz"]),
  sourceCommit: z.string(),
  hostId: z.string(),
  roles: z.array(roleState).length(6),
  network: z.strictObject({
    generation: z.string().regex(/^[A-Za-z0-9_-]{1,96}$/),
    enforcementSha256: hash,
    fixtureOnlyEgress: z.literal(true),
    externalDnsBlocked: z.literal(true),
    roles: z.array(z.enum(["orchestrator", "coordinator", "worker"])).length(3),
  }),
});
export type SupervisorStatus = z.infer<typeof StatusSchema>;

/** The adapter implements these fixed operations. No commands, paths or arbitrary URLs cross this socket. */
export class Supervisor {
  constructor(private readonly ctx: Context) {}
  private async call(
    operation: "status" | "restart" | "stop" | "infrastructure-ready",
    role?: MutableRole,
    configurationId?: ConfigurationId,
  ): Promise<SupervisorStatus> {
    if ((operation === "restart" || operation === "stop") && !role)
      throw new RunnerFailure("INVALID_CONTEXT", "restart_domain");
    const token = (
      await privateFile(this.ctx.runRoot, this.ctx.supervisor.tokenFile)
    )
      .toString("utf8")
      .trim();
    requireThat(/^[A-Za-z0-9_-]{32,128}$/.test(token), "supervisor_token");
    const body = JSON.stringify({
      version: 1,
      runId: this.ctx.runId,
      operation,
      ...(role ? { role } : {}),
      ...(configurationId ? { configurationId } : {}),
    });
    const value = await new Promise<unknown>((resolve, reject) => {
      const fail = () =>
        reject(new RunnerFailure("TRANSPORT_FAILED", "supervisor"));
      const req = request(
        {
          socketPath: this.ctx.supervisor.socket,
          path: "/v1/control",
          method: "POST",
          agent: false,
          headers: {
            authorization: `Bearer ${token}`,
            "content-type": "application/json",
            "content-length": Buffer.byteLength(body),
          },
        },
        (res) => {
          const chunks: Buffer[] = [];
          let bytes = 0;
          res.on("data", (chunk: Buffer) => {
            bytes += chunk.length;
            if (bytes > 65536) req.destroy();
            else chunks.push(chunk);
          });
          res.on("error", fail);
          res.on("end", () => {
            clearTimeout(timer);
            try {
              if (res.statusCode !== 200) throw new Error();
              resolve(JSON.parse(Buffer.concat(chunks).toString("utf8")));
            } catch {
              fail();
            }
          });
        },
      );
      const timer = setTimeout(
        () => req.destroy(),
        this.ctx.deadlines.requestMs,
      );
      req.on("error", () => {
        clearTimeout(timer);
        fail();
      });
      req.end(body);
    });
    const parsed = StatusSchema.safeParse(value);
    requireThat(parsed.success, "supervisor_response");
    const state = parsed.data;
    requireThat(
      state.runId === this.ctx.runId &&
        state.backend === this.ctx.backend &&
        state.sourceCommit === this.ctx.sourceCommit &&
        state.hostId === this.ctx.hostId,
      "supervisor_identity",
    );
    requireThat(
      new Set(state.roles.map((v) => v.role)).size === 6 &&
        new Set(state.network.roles).size === 3,
      "supervisor_role_set",
    );
    const artifacts = {
      orchestrator: this.ctx.artifacts.orchestrator,
      worker: this.ctx.artifacts.worker,
      coordinator: this.ctx.artifacts.coordinator,
      "host-agent": this.ctx.artifacts.hostAgent,
      "fixture-modern": this.ctx.artifacts.fixture,
      "fixture-legacy": this.ctx.artifacts.fixture,
    };
    for (const entry of state.roles)
      requireThat(
        entry.artifactSha256 === artifacts[entry.role].sha256,
        "supervisor_artifact",
      );
    for (const entry of state.roles) {
      if (entry.role === "fixture-modern" || entry.role === "fixture-legacy") {
        requireThat(
          entry.configurationId === "normal" &&
            entry.configurationSha256 ===
              this.ctx.protocolFixtures[
                entry.role === "fixture-modern" ? "modern" : "legacy"
              ].configuration.sha256,
          "fixture_configuration",
        );
      } else {
        requireThat(
          entry.role === "orchestrator" || entry.configurationId === "normal",
          "only_orchestrator_has_disable_variants",
        );
        const configuration =
          entry.role === "orchestrator"
            ? this.ctx.supervisor.configurations.orchestrator[
                entry.configurationId
              ]
            : this.ctx.supervisor.configurations[entry.role].normal;
        requireThat(
          configuration && configuration.sha256 === entry.configurationSha256,
          "supervisor_configuration",
        );
      }
    }
    return state;
  }
  async infrastructureReady() {
    return this.call("infrastructure-ready");
  }
  async status() {
    return this.call("status");
  }
  async restart(
    role: MutableRole,
    configurationId: ConfigurationId = "normal",
  ) {
    requireThat(
      role === "orchestrator"
        ? this.ctx.supervisor.configurations.orchestrator[configurationId]
        : configurationId === "normal",
      "declared_configuration",
    );
    const before = await this.status();
    requireThat(
      before.roles.every((r) => r.state === "running"),
      "roles_running_before_restart",
    );
    const after = await this.call("restart", role, configurationId);
    requireThat(
      after.roles.find((r) => r.role === role)?.configurationId ===
        configurationId,
      "selected_configuration",
    );
    requireThat(
      after.roles.every((r) => r.state === "running"),
      "roles_running_after_restart",
    );
    for (const current of after.roles) {
      const prior = before.roles.find((r) => r.role === current.role)!;
      requireThat(
        current.role === role
          ? current.bootId !== prior.bootId
          : current.bootId === prior.bootId,
        "restart_scope",
      );
    }
    requireThat(
      after.network.generation === before.network.generation &&
        after.network.enforcementSha256 === before.network.enforcementSha256,
      "restart_network_preserved",
    );
    return after;
  }
  async stop(role: MutableRole) {
    return this.call("stop", role);
  }
}
