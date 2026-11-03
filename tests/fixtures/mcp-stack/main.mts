import { timingSafeEqual } from "node:crypto";
import { createServer } from "node:https";
import { caseOptions, keys, loadConfig, object } from "./config.mts";
import { openEffects } from "./durable.mts";
import { startMcpFixture } from "../../../mcp-worker/test/fixtures/server.ts";

async function main() {
  if (process.argv.length !== 3) throw new Error("Expected config path");
  const config = loadConfig(process.argv[2]!);
  const effects = openEffects(config.root);
  const fixture = await startMcpFixture({
    ...config.options,
    initialCount: effects.snapshot().count,
    commitWrite: (id, delta) => effects.commit(id, delta),
  });
  if (config.options.revision !== undefined)
    fixture.setCatalogRevision(config.options.revision);
  let closing: Promise<void> | undefined;
  const control = createServer(
    { key: config.options.network.key, cert: config.options.network.cert },
    (req, res) => {
      void (async () => {
        const respond = (value: unknown, status = 200) => {
          res
            .writeHead(status, {
              "content-type": "application/json",
              "cache-control": "no-store",
            })
            .end(JSON.stringify(value));
        };
        const expected = Buffer.from(`Bearer ${config.token}`);
        const supplied = Buffer.from(req.headers.authorization ?? "");
        if (
          expected.length !== supplied.length ||
          !timingSafeEqual(expected, supplied)
        ) {
          respond({ error: "unauthorized" }, 401);
          return;
        }
        if (req.headers.host !== new URL(config.control.origin).host) {
          respond({ error: "authority" }, 421);
          return;
        }
        const route = req.url;
        if (req.method === "GET" && route === "/v1/ready") {
          respond({
            version: 1,
            ready: !closing && effects.healthy(),
            resource: fixture.url,
            issuer: config.options.network.oauth.issuer,
          });
          return;
        }
        if (req.method === "GET" && route === "/v1/observe") {
          respond({
            version: 1,
            healthy: effects.healthy(),
            effects: effects.snapshot(),
            reads: fixture.reads,
            held: fixture.heldCalls,
            cancellations: fixture.cancellations,
            oauth: fixture.oauth.observe(),
          });
          return;
        }
        if (
          req.method !== "POST" ||
          ![
            "/v1/case",
            "/v1/release",
            "/v1/cancel",
            "/v1/oauth",
            "/v1/shutdown",
          ].includes(route ?? "")
        ) {
          respond({ error: "unknown operation" }, 404);
          return;
        }
        let bytes = 0;
        const chunks: Buffer[] = [];
        for await (const chunk of req) {
          bytes += Buffer.byteLength(chunk);
          if (bytes > 4096) {
            respond({ error: "body limit" }, 413);
            return;
          }
          chunks.push(Buffer.from(chunk));
        }
        const body: unknown = JSON.parse(
          Buffer.concat(chunks).toString("utf8"),
        );
        object(body);
        if (route === "/v1/case") {
          const value = caseOptions(body);
          if (fixture.heldCalls) {
            respond({ error: "held calls" }, 409);
            return;
          }
          fixture.setCase(value);
          if (value.revision !== undefined)
            fixture.setCatalogRevision(value.revision);
        } else if (route === "/v1/oauth") {
          keys(body, ["account", "scope", "issuer", "expiresInSeconds"], []);
          if (
            body.account !== undefined &&
            (typeof body.account !== "string" ||
              !["account-one", "account-two"].includes(body.account))
          )
            throw new Error("Invalid account");
          if (
            body.scope !== undefined &&
            (typeof body.scope !== "string" ||
              !["tools:read", "tools:write", "tools:read tools:write"].includes(
                body.scope,
              ))
          )
            throw new Error("Invalid scope");
          if (
            body.issuer !== undefined &&
            body.issuer !== "configured" &&
            body.issuer !== "mismatch"
          )
            throw new Error("Invalid issuer mode");
          if (
            body.expiresInSeconds !== undefined &&
            (typeof body.expiresInSeconds !== "number" ||
              !Number.isInteger(body.expiresInSeconds) ||
              body.expiresInSeconds < 1 ||
              body.expiresInSeconds > 3600)
          )
            throw new Error("Invalid token lifetime");
          const value: {
            expiresInSeconds?: number;
            account?: string;
            scope?: string;
            issuer?: "configured" | "mismatch";
          } = {};
          if (typeof body.expiresInSeconds === "number")
            value.expiresInSeconds = body.expiresInSeconds;
          if (body.issuer === "configured" || body.issuer === "mismatch")
            value.issuer = body.issuer;
          if (typeof body.account === "string") value.account = body.account;
          if (typeof body.scope === "string") value.scope = body.scope;
          fixture.oauth.configure(value);
        } else {
          keys(body, []);
          if (route === "/v1/release") fixture.releaseCalls();
          if (route === "/v1/cancel") fixture.cancelCalls();
        }
        respond({ ok: true });
        if (route === "/v1/shutdown")
          setImmediate(() => {
            void stop();
          });
      })().catch(() => {
        if (!res.headersSent)
          res.writeHead(400, { "content-type": "application/json" });
        res.end('{"error":"invalid operation"}');
      });
    },
  );
  control.requestTimeout = 5000;
  control.headersTimeout = 5000;
  control.keepAliveTimeout = 1000;
  control.maxConnections = 32;
  async function stop() {
    closing ??= (async () => {
      control.closeAllConnections();
      await Promise.all([
        fixture.close(),
        new Promise<void>((resolve, reject) =>
          control.close((error) => (error ? reject(error) : resolve())),
        ),
      ]);
    })();
    await closing;
  }
  try {
    await new Promise<void>((resolve, reject) => {
      control.once("error", reject);
      control.listen(config.control.port, config.control.host, resolve);
    });
  } catch {
    await fixture.close();
    throw new Error("Control listener failed");
  }
  const shutdown = () => {
    void stop().catch(() => {
      process.exitCode = 1;
    });
  };
  process.once("SIGTERM", shutdown);
  process.once("SIGINT", shutdown);
  process.stdout.write(
    JSON.stringify({
      version: 1,
      kind: "ready",
      control: config.control.origin,
      resource: fixture.url,
    }) + "\n",
  );
}
void main().catch(() => {
  process.stderr.write('{"version":1,"kind":"startup_failed"}\n');
  process.exitCode = 1;
});
