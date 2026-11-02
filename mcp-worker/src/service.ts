import { parseWorkerRequest } from "./validation.ts";
import { createServer } from "node:https";
import type { ServerOptions } from "node:https";
import type { IncomingMessage } from "node:http";
import { TLSSocket } from "node:tls";
import { randomUUID } from "node:crypto";
import { resolve4, resolve6 } from "node:dns/promises";
import { ZonePolicy, restrictedFetch, abortable } from "./network.ts";
import type { NetworkDependencies } from "./network.ts";
import { digest, object } from "./encoding.ts";
import { runOperation } from "./engine.ts";
import type {
  WorkerIdentity,
  WorkerRequest,
  WorkerOperation,
  WorkerResponse,
  WorkerIntent,
} from "./contracts.ts";
export interface WorkerOptions {
  zone: ZonePolicy;
  tls: Pick<ServerOptions, "key" | "cert" | "ca">;
  callers: { fingerprint: string; role: "orchestrator" | "coordinator" }[];
  enabled?: boolean;
  network?: NetworkDependencies;
}
export function createWorker(options: WorkerOptions) {
  if (Number(process.versions.node.split(".")[0]) !== 24)
    throw Error("node_24_required");
  const identity: WorkerIdentity = {
    protocol: 1,
    bootId: randomUUID(),
    zoneId: options.zone.config.id,
    policyDigest: options.zone.digest,
    policyRevision: options.zone.config.revision,
  };
  const used = new Map<string, number>();
  const leases = new Map<string, number>();
  const active = new Map<
    string,
    {
      controller: AbortController;
      intent: WorkerIntent;
      principal: string;
      bucket: string;
    }
  >();
  const fetch = restrictedFetch(options.zone, options.network);
  const server = createServer(
    {
      ...options.tls,
      requestCert: true,
      rejectUnauthorized: true,
      minVersion: "TLSv1.3",
      maxHeaderSize: 8192,
    },
    (req, res) => {
      void handle(req).then(
        ({ status, body }) => {
          res.writeHead(status, {
            "content-type": "application/json",
            "cache-control": "no-store",
          });
          res.end(JSON.stringify(body));
        },
        () => {
          res.writeHead(400, { "content-type": "application/json" });
          res.end(JSON.stringify({ ok: false, error: "invalid_request" }));
        },
      );
    },
  );
  server.maxConnections = 128;
  server.requestTimeout = 5000;
  server.headersTimeout = 5000;
  server.timeout = 305000;
  server.maxRequestsPerSocket = 1;
  async function handle(
    req: IncomingMessage,
  ): Promise<{ status: number; body: unknown }> {
    const deny = (status: number, error: string) => ({
      status,
      body: { ok: false, error },
    });
    if (!(req.socket instanceof TLSSocket) || !req.socket.authorized)
      return deny(403, "caller_denied");
    const principal = req.socket.getPeerCertificate().fingerprint256;
    const caller = options.callers.find((c) => c.fingerprint === principal);
    if (!caller) return deny(403, "caller_denied");
    if (req.method === "GET" && req.url === "/identity")
      return { status: 200, body: identity };
    if (req.method !== "POST" || req.url !== "/rpc")
      return deny(404, "route_denied");
    if (!options.enabled) return deny(503, "worker_disabled");
    const request = parseWorkerRequest(await readBody(req));
    const { intent, operation } = request;
    const now = Date.now();
    for (const [id, expiry] of used) if (expiry <= now) used.delete(id);
    for (const [id, expiry] of leases) if (expiry <= now) leases.delete(id);
    if (
      intent.bootId !== identity.bootId ||
      intent.zoneId !== identity.zoneId ||
      intent.policyDigest !== identity.policyDigest ||
      intent.operation !== operation.kind ||
      intent.destination !==
        ("endpoint" in operation ? operation.endpoint : "") ||
      intent.operationDigest !== digest(operation) ||
      intent.expiresAt <= now ||
      intent.expiresAt > now + 300000 ||
      intent.lease.expiresAt < intent.expiresAt ||
      used.has(intent.id) ||
      leases.has(intent.lease.id) ||
      used.size >= 4096 ||
      leases.size >= 4096 ||
      intent.lease.expiresAt > now + 300000
    )
      return deny(403, "intent_denied");
    const allowed =
      caller.role === "coordinator"
        ? ["metadata", "token", "validateDestination", "cancel"]
        : [
            "discover",
            "execute",
            "cancel",
            "validateSchemas",
            "validateDestination",
          ];
    if (!allowed.includes(operation.kind)) return deny(403, "operation_denied");
    if ("endpoint" in operation) options.zone.endpoint(operation.endpoint);
    // Consume before admission or I/O. Failed admission needs a new intent.
    used.set(intent.id, intent.expiresAt);
    leases.set(intent.lease.id, intent.lease.expiresAt);
    const bucket =
      operation.kind === "execute"
        ? "execute"
        : operation.kind === "discover"
          ? "discover"
          : operation.kind === "metadata" || operation.kind === "token"
            ? "oauth"
            : operation.kind === "cancel"
              ? "cancel"
              : "schema";
    const limits = options.zone.config.limits;
    const caps = {
      execute: Math.min(limits?.execute ?? 32, 32),
      discover: Math.min(limits?.discover ?? 2, 8),
      oauth: Math.min(limits?.oauth ?? 4, 8),
      schema: Math.min(limits?.schema ?? 4, 8),
      cancel: 32,
    };
    const running = [...active.values()];
    if (
      running.filter((a) => a.bucket === bucket).length >= caps[bucket] ||
      (bucket === "execute" &&
        (running.filter(
          (a) =>
            a.bucket === "execute" &&
            a.intent.scope.kind === "execution" &&
            intent.scope.kind === "execution" &&
            a.intent.scope.sessionId === intent.scope.sessionId,
        ).length >= 4 ||
          running.filter(
            (a) =>
              a.bucket === "execute" &&
              a.intent.connectionId === intent.connectionId,
          ).length >= 8))
    )
      return deny(429, "capacity_exceeded");
    const controller = new AbortController();
    const disconnected = () => controller.abort();
    req.socket.once("close", disconnected);
    const maxMs =
      bucket === "execute"
        ? Math.min(limits?.toolMs ?? 60000, 300000)
        : bucket === "cancel"
          ? 5000
          : 10000;
    const timer = setTimeout(
      () => controller.abort(),
      Math.min(intent.expiresAt - now, maxMs),
    );
    active.set(intent.id, { controller, intent, principal, bucket });
    try {
      let result: unknown;
      if (operation.kind === "cancel") {
        const target = active.get(operation.targetIntentId);
        if (
          target &&
          (target.principal !== principal ||
            digest(target.intent.scope) !== digest(intent.scope) ||
            target.intent.connectionId !== intent.connectionId ||
            target.intent.connectionRevision !== intent.connectionRevision ||
            target.intent.credentialIdentity !== intent.credentialIdentity)
        )
          return deny(403, "cancel_denied");
        target?.controller.abort();
        result = { cancelRequested: !!target };
      } else if (operation.kind === "validateDestination") {
        const u = options.zone.endpoint(operation.endpoint);
        const addresses = options.network
          ? await abortable(
              options.network.resolve(u.hostname),
              controller.signal,
            )
          : (
              await abortable(
                Promise.all(
                  [resolve4(u.hostname), resolve6(u.hostname)].map(
                    async (p) => {
                      try {
                        return await p;
                      } catch (e) {
                        if (
                          e instanceof Error &&
                          "code" in e &&
                          ["ENODATA", "ENOTFOUND"].includes(String(e.code))
                        )
                          return [];
                        throw Error("dns_failed");
                      }
                    },
                  ),
                ),
                controller.signal,
              )
            ).flat();
        options.zone.checkAddresses(addresses);
        result = { approved: true };
      } else result = await runOperation(operation, fetch, controller.signal);
      const body: WorkerResponse = {
        ok: true,
        identity,
        intent,
        intentId: intent.id,
        requestDigest: intent.requestDigest,
        operationDigest: intent.operationDigest,
        leaseId: intent.lease.id,
        result,
      };
      return { status: 200, body };
    } catch {
      return deny(422, "operation_failed");
    } finally {
      clearTimeout(timer);
      req.socket.removeListener("close", disconnected);
      controller.abort();
      active.delete(intent.id);
    }
  }
  return {
    identity,
    async listen(port = 8443, host = "0.0.0.0") {
      await new Promise<void>((resolve, reject) => {
        server.once("error", reject);
        server.listen(port, host, resolve);
      });
      const a = server.address();
      if (!a || typeof a === "string") throw Error("listen_failed");
      return a.port;
    },
    async close() {
      for (const item of active.values()) item.controller.abort();
      server.closeAllConnections();
      await new Promise<void>((resolve, reject) =>
        server.close((e) => (e ? reject(e) : resolve())),
      );
    },
  };
}
async function readBody(req: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  let length = 0;
  for await (const chunk of req) {
    const part = Buffer.from(chunk);
    length += part.length;
    if (length > 1536 * 1024) throw Error("request_limit");
    chunks.push(part);
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}
