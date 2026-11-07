import {
  createServer,
  type IncomingMessage,
  type ServerResponse,
} from "node:http";
import { constants } from "node:fs";
import { chmod, lstat, open, unlink } from "node:fs/promises";
import { dirname, join, sep } from "node:path";
import {
  ROLES,
  StatusSchema,
  type MutableRole,
  type SupervisorStatus,
} from "../../../../orchestrator/scripts/mcp-stack/supervisor.ts";
import type {
  ConfigurationId,
  Context,
} from "../../../../orchestrator/scripts/mcp-stack/context.ts";
import {
  check,
  ownedPath,
  PlatformFailure,
  privateRoot,
  readOwned,
  record,
  sameSecret,
  verifyPin,
} from "./owned.ts";

export type Operation = { version: 1; runId: string } & (
  | { operation: "status" }
  | { operation: "stop"; role: MutableRole }
  | {
      operation: "restart";
      role: MutableRole;
      configurationId: ConfigurationId;
    }
);
export function parseOperation(value: unknown, ctx: Context): Operation {
  const base = record(
    value,
    ["version", "runId", "operation"],
    ["role", "configurationId"],
  );
  check(base.version === 1 && base.runId === ctx.runId, "supervisor_run");
  if (base.operation === "status") {
    record(base, ["version", "runId", "operation"]);
    return { version: 1, runId: ctx.runId, operation: "status" };
  }
  check(
    base.operation === "restart" || base.operation === "stop",
    "supervisor_operation",
  );
  check(
    base.role === "orchestrator" ||
      base.role === "worker" ||
      base.role === "coordinator" ||
      base.role === "host-agent",
    "supervisor_role",
  );
  const role = base.role;
  if (base.operation === "stop") {
    record(base, ["version", "runId", "operation", "role"]);
    return { version: 1, runId: ctx.runId, operation: "stop", role };
  }
  record(base, ["version", "runId", "operation", "role", "configurationId"]);
  check(
    base.configurationId === "normal" ||
      base.configurationId === "global-disabled" ||
      base.configurationId === "zone-disabled",
    "supervisor_configuration",
  );
  const configurationId = base.configurationId;
  check(
    role === "orchestrator" || configurationId === "normal",
    "supervisor_role_configuration",
  );
  check(
    role === "orchestrator"
      ? ctx.supervisor.configurations.orchestrator[configurationId]
      : ctx.supervisor.configurations[role].normal,
    "supervisor_configuration",
  );
  return {
    version: 1,
    runId: ctx.runId,
    operation: "restart",
    role,
    configurationId,
  };
}

// Only this fixed lifecycle is callable. The adapter supplies owned handles, never request-defined commands.
export type Lifecycle = {
  status(signal: AbortSignal): Promise<SupervisorStatus>;
  restart(
    role: MutableRole,
    configuration: ConfigurationId,
    signal: AbortSignal,
  ): Promise<void>;
  stop(role: MutableRole, signal: AbortSignal): Promise<void>;
};
export function verifyStatus(ctx: Context, value: unknown): SupervisorStatus {
  const status = StatusSchema.parse(value);
  check(
    status.runId === ctx.runId &&
      status.backend === ctx.backend &&
      status.sourceCommit === ctx.sourceCommit &&
      status.hostId === ctx.hostId,
    "supervisor_identity",
  );
  check(
    new Set(status.roles.map((v) => v.role)).size === ROLES.length &&
      new Set(status.network.roles).size === 3,
    "supervisor_roles",
  );
  const artifacts = {
    orchestrator: ctx.artifacts.orchestrator,
    worker: ctx.artifacts.worker,
    coordinator: ctx.artifacts.coordinator,
    "host-agent": ctx.artifacts.hostAgent,
    "fixture-modern": ctx.artifacts.fixture,
    "fixture-legacy": ctx.artifacts.fixture,
  };
  for (const role of status.roles) {
    check(
      role.role === "orchestrator" || role.configurationId === "normal",
      "supervisor_role_configuration",
    );
    const config =
      role.role === "fixture-modern" || role.role === "fixture-legacy"
        ? role.configurationId === "normal"
          ? ctx.protocolFixtures[
              role.role === "fixture-modern" ? "modern" : "legacy"
            ].configuration
          : undefined
        : role.role === "orchestrator"
          ? ctx.supervisor.configurations.orchestrator[role.configurationId]
          : ctx.supervisor.configurations[role.role].normal;
    check(
      config &&
        config.sha256 === role.configurationSha256 &&
        artifacts[role.role].sha256 === role.artifactSha256,
      "supervisor_hash",
    );
  }
  return status;
}
export async function startSupervisor(ctx: Context, lifecycle: Lifecycle) {
  await privateRoot(ctx.runRoot);
  check(
    ctx.supervisor.socket.startsWith(ctx.runRoot + sep) &&
      ctx.supervisor.tokenFile.startsWith(ctx.runRoot + sep),
    "supervisor_scope",
  );
  await privateRoot(dirname(ctx.supervisor.socket));
  await ownedPath(ctx.supervisor.socket, true);
  const token = (await readOwned(ctx.supervisor.tokenFile, 256))
    .toString()
    .trim();
  check(/^[A-Za-z0-9_-]{32,128}$/.test(token), "supervisor_token");
  let busy = false,
    closing = false;
  let active:
    | {
        controller: AbortController;
        settled: Promise<void>;
        diagnosed: Promise<void>;
      }
    | undefined;
  const sockets = new Set<import("node:net").Socket>();
  let failureNumber = 0;
  const safeSeams = new Set([
    "role_readiness",
    "engine_command",
    "engine_deadline",
    "container_stop",
    "container_survivor",
    "role_start",
    "role_configuration",
    "role_handle",
    "role_identity",
    "role_running",
    "running_artifact",
    "artifact_identity",
    "artifact_hash",
    "supervisor_configuration",
    "roles_before_restart",
    "restart_network",
    "restart_scope",
    "restart_identity",
    "stop_identity",
    "supervisor_identity",
    "supervisor_roles",
    "supervisor_hash",
    "control_aborted",
    "worker_readiness_status",
    "worker_readiness_tls",
    "worker_readiness_identity",
    "orchestrator_readiness",
    "coordinator_readiness",
  ]);
  function reply(res: ServerResponse, status: number, body: unknown) {
    if (res.destroyed) return;
    const bytes = JSON.stringify(body);
    if (Buffer.byteLength(bytes) > 65536) {
      res.destroy();
      return;
    }
    res.writeHead(status, {
      "content-type": "application/json",
      "cache-control": "no-store",
      connection: "close",
    });
    res.end(bytes);
  }
  async function serve(req: IncomingMessage, res: ServerResponse) {
    if (closing) return reply(res, 503, { error: "closing" });
    if (req.method !== "POST" || req.url !== "/v1/control")
      return reply(res, 404, { error: "route" });
    // Duplicate auth/length/type fields are refused before body parsing.
    const names = req.rawHeaders
      .filter((_v, i) => i % 2 === 0)
      .map((v) => v.toLowerCase());
    if (
      names.filter((v) => v === "authorization").length !== 1 ||
      !sameSecret(req.headers.authorization ?? "", `Bearer ${token}`)
    )
      return reply(res, 401, { error: "authentication" });
    if (
      names.filter((v) => v === "content-type").length !== 1 ||
      names.filter((v) => v === "content-length").length !== 1 ||
      req.headers["content-type"] !== "application/json" ||
      req.headers["transfer-encoding"] ||
      !/^[1-9][0-9]{0,3}$/.test(req.headers["content-length"] ?? "") ||
      Number(req.headers["content-length"]) > 4096
    )
      return reply(res, 400, { error: "request" });
    if (busy) return reply(res, 409, { error: "busy" });
    busy = true;
    const controller = new AbortController();
    const { signal } = controller;
    let settle!: () => void;
    const settled = new Promise<void>((resolve) => {
      settle = resolve;
    });
    let finishDiagnostic!: () => void;
    const diagnosed = new Promise<void>((resolve) => {
      finishDiagnostic = resolve;
    });
    active = { controller, settled, diagnosed };
    const aborted = new Promise<never>((_resolve, reject) => {
      signal.addEventListener(
        "abort",
        () => reject(new PlatformFailure("FAILED", "control_aborted")),
        { once: true },
      );
    });
    const timer = setTimeout(
      () => controller.abort("deadline"),
      ctx.deadlines.requestMs,
    );
    const disconnect = () => {
      if (!res.writableEnded) controller.abort("disconnect");
    };
    res.once("close", disconnect);
    let lifecycleStarted = false;
    let operation: Operation | undefined;
    let phase:
      | "request"
      | "configuration"
      | "before_status"
      | "lifecycle"
      | "after_status"
      | "transition"
      | "reply" = "request";
    async function execute() {
      const chunks: Buffer[] = [];
      let length = 0;
      for await (const chunk of req) {
        const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
        length += bytes.length;
        if (length > 4096)
          throw new PlatformFailure("INVALID_CONTEXT", "body_size");
        chunks.push(bytes);
      }
      check(length === Number(req.headers["content-length"]), "body_length");
      operation = parseOperation(
        JSON.parse(Buffer.concat(chunks).toString("utf8")),
        ctx,
      );
      if (operation.operation === "restart") {
        phase = "configuration";
        const configuration =
          operation.role === "orchestrator"
            ? ctx.supervisor.configurations.orchestrator[
                operation.configurationId
              ]
            : ctx.supervisor.configurations[operation.role].normal;
        check(configuration, "supervisor_configuration");
        await verifyPin(configuration);
      }
      signal.throwIfAborted();
      lifecycleStarted = true;
      phase = "before_status";
      const before = verifyStatus(ctx, await lifecycle.status(signal));
      signal.throwIfAborted();
      if (operation.operation === "status") {
        phase = "reply";
        return reply(res, 200, before);
      }
      if (operation.operation === "restart") {
        check(
          before.roles.every((r) => r.state === "running"),
          "roles_before_restart",
        );
        phase = "lifecycle";
        await lifecycle.restart(
          operation.role,
          operation.configurationId,
          signal,
        );
      } else {
        phase = "lifecycle";
        await lifecycle.stop(operation.role, signal);
      }
      signal.throwIfAborted();
      phase = "after_status";
      const after = verifyStatus(ctx, await lifecycle.status(signal));
      signal.throwIfAborted();
      phase = "transition";
      check(
        before.network.generation === after.network.generation &&
          before.network.enforcementSha256 === after.network.enforcementSha256,
        "restart_network",
      );
      for (const prior of before.roles) {
        const current = after.roles.find((r) => r.role === prior.role)!;
        if (current.role !== operation.role)
          check(
            current.bootId === prior.bootId &&
              current.state === prior.state &&
              current.configurationId === prior.configurationId &&
              current.configurationSha256 === prior.configurationSha256,
            "restart_scope",
          );
        else if (operation.operation === "restart")
          check(
            current.state === "running" &&
              current.bootId !== prior.bootId &&
              current.configurationId === operation.configurationId,
            "restart_identity",
          );
        else check(current.state === "stopped", "stop_identity");
      }
      phase = "reply";
      reply(res, 200, after);
    }
    const execution = execute().finally(settle);
    try {
      await Promise.race([execution, aborted]);
    } catch (error) {
      // A timed-out or failed lifecycle can have changed state. Refuse further controls.
      if (lifecycleStarted) closing = true;
      if (signal.aborted) req.destroy();
      if (operation) {
        const reason =
          signal.aborted &&
          ["deadline", "disconnect", "shutdown"].includes(signal.reason)
            ? (signal.reason as "deadline" | "disconnect" | "shutdown")
            : undefined;
        const known =
          error instanceof PlatformFailure &&
          ["FAILED", "INVALID_CONTEXT", "NEEDS_CONTEXT"].includes(error.code) &&
          safeSeams.has(error.seam);
        const diagnostic = {
          version: 1,
          operation: operation.operation,
          ...(operation.operation === "status" ? {} : { role: operation.role }),
          phase,
          ...(reason
            ? { failure: reason, seam: "control_aborted" }
            : {
                failure: known ? error.code : "OTHER",
                seam: known ? error.seam : "other",
              }),
        };
        try {
          const file = await open(
            join(ctx.runRoot, `supervisor-failure-${++failureNumber}.json`),
            constants.O_WRONLY |
              constants.O_CREAT |
              constants.O_EXCL |
              constants.O_NOFOLLOW,
            0o600,
          );
          try {
            await file.writeFile(JSON.stringify(diagnostic) + "\n");
            await file.sync();
          } finally {
            await file.close();
          }
        } catch {
          /* Diagnostics must not change the control response. */
        }
      }
      if (!signal.aborted) reply(res, 400, { error: "control_failed" });
    } finally {
      finishDiagnostic();
      clearTimeout(timer);
      res.off("close", disconnect);
      if (!lifecycleStarted || !signal.aborted) active = undefined;
      busy = false;
    }
  }
  const server = createServer(
    {
      maxHeaderSize: 4096,
      requestTimeout: 5000,
      headersTimeout: 5000,
      connectionsCheckingInterval: 500,
    },
    (req, res) => {
      void serve(req, res);
    },
  );
  server.maxConnections = 8;
  server.keepAliveTimeout = 1;
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.setTimeout(ctx.deadlines.requestMs, () => socket.destroy());
    socket.once("close", () => sockets.delete(socket));
  });
  server.on("clientError", (_error, socket) => socket.destroy());
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(ctx.supervisor.socket, resolve);
  });
  await chmod(ctx.supervisor.socket, 0o600);
  const socketStat = await lstat(ctx.supervisor.socket);
  return {
    async close() {
      closing = true;
      active?.controller.abort("shutdown");
      for (const socket of sockets) socket.destroy();
      // Closing a Unix listener can unlink its pathname. Check before that operation.
      // On substitution retain the descriptor until process exit; report failure, not cleanup.
      try {
        const current = await lstat(ctx.supervisor.socket);
        check(
          current.ino === socketStat.ino &&
            current.dev === socketStat.dev &&
            current.isSocket(),
          "socket_replaced",
        );
      } catch (error) {
        server.unref();
        throw error;
      }
      await new Promise<void>((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      );
      if (active) {
        let timer: ReturnType<typeof setTimeout> | undefined;
        try {
          await Promise.race([
            Promise.all([active.settled, active.diagnosed]),
            new Promise<never>((_resolve, reject) => {
              timer = setTimeout(
                () => reject(new PlatformFailure("FAILED", "control_cleanup")),
                ctx.deadlines.requestMs,
              );
            }),
          ]);
        } finally {
          clearTimeout(timer);
        }
      }
      // Node removes its own Unix socket. Never unlink a substituted path.
      try {
        const current = await lstat(ctx.supervisor.socket);
        check(
          current.ino === socketStat.ino &&
            current.dev === socketStat.dev &&
            current.isSocket(),
          "socket_replaced",
        );
        await unlink(ctx.supervisor.socket);
      } catch (e) {
        if (!(e instanceof Error && "code" in e && e.code === "ENOENT"))
          throw e;
      }
    },
  };
}
