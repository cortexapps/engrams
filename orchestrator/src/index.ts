import { Hono } from "hono";
import { createNodeWebSocket } from "@hono/node-ws";
import { config } from "./config.ts";
import { buildServer } from "./server.ts";
import health from "./routes/health.ts";
import authRoute from "./routes/auth.ts";
import eventsRoute from "./routes/events.ts";
import artifactsRoute from "./routes/artifacts.ts";
import meRoute from "./routes/me.ts";
import adminRoute from "./routes/admin.ts";
import skillsRoute from "./routes/skills.ts";
import { makeShellRoute } from "./routes/shell.ts";
import { registerPassthrough } from "./rpc/passthrough.ts";
import { makeDisableImageGuard } from "./rpc/image-guard.ts";
import { registerTasks } from "./rpc/tasks.ts";
import { registerProfiles } from "./rpc/profiles.ts";
import { SURFACE } from "./rpc/surface.ts";
import { controlPlaneTransport } from "./control-plane/transport.ts";
import type { ConnectRouter } from "@connectrpc/connect";

const app = new Hono();

// ADR 0051 Task 21: WebSocket shell route via @hono/node-ws.
// createNodeWebSocket must be called with the Hono app BEFORE routes are mounted
// so injectWebSocket can install the upgrade handler on the node:http server.
const { upgradeWebSocket, injectWebSocket, wss } = createNodeWebSocket({ app });

// Echo 'tty' subprotocol — browsers hard-fail without this.
// handleProtocols is read by ws's handleUpgrade on each connection.
wss.options.handleProtocols = (protocols: Set<string>) =>
  protocols.has("tty") ? "tty" : false;

// Mount routes.
app.route("/", health);
app.route("/", authRoute);
// ADR 0051 Task 20: browser-native HTTP legs (SSE events, artifact bytes, /me/claude-token).
app.route("/", eventsRoute);
app.route("/", artifactsRoute);
app.route("/", meRoute);
// ADR 0051 Task 28: admin REST proxy (pause/resume session — no gRPC equiv yet).
app.route("/", adminRoute);
// ADR 0055 P2: skill catalog list/upload/delete (multipart upload is HTTP, not gRPC).
app.route("/", skillsRoute);

// ADR 0051 Task 21: Shell WebSocket route.
const { app: shellApp, injectUpgrade } = makeShellRoute();
injectUpgrade(upgradeWebSocket);
app.route("/", shellApp);

// Default 404 for unmatched Hono paths.
app.notFound((c) => c.json({ error: "not found" }, 404));

const server = buildServer(
  app,
  (router: ConnectRouter) => {
    // Native TaskService: orchestrator-owned task model (ADR 0051 §3, Task 19).
    // Registered BEFORE the passthrough so it wins the /rpc/engram.app.v1.TaskService/* prefix.
    registerTasks(router);

    // Native ProfileService: orchestrator-owned session profiles (ADR 0052).
    registerProfiles(router);

    // Generic passthrough: forwards SessionService, FleetService, ImageService
    // to the control plane with per-method CASL authz gate (ADR 0051 Task 18).
    // ADR 0052 Task 8: a DisableImage pre-flight blocks disabling an image that
    // any active profile still references (the coordinator only knows sessions).
    registerPassthrough(router, SURFACE, controlPlaneTransport, undefined, undefined, {
      "ImageService.DisableImage": makeDisableImageGuard(),
    });
  },
  // Pass the full NodeWebSocket handle so buildServer can install the
  // Bun-compatible upgrade handler (wss.handleUpgrade instead of socket.end).
  // injectWebSocket is included for completeness but the custom upgrade handler
  // is used instead of calling nodeWs.injectWebSocket(server).
  { upgradeWebSocket, wss, injectWebSocket },
);

server.listen(config.port, "0.0.0.0", () => {
  console.log(`Orchestrator listening on port ${config.port}`);
});

// Graceful shutdown on SIGTERM (e.g. Tilt stop, Kubernetes pod termination).
process.on("SIGTERM", () => {
  console.log("Orchestrator: SIGTERM received, shutting down gracefully…");
  server.close((err) => {
    if (err) {
      console.error("Orchestrator: error during shutdown", err);
      process.exit(1);
    }
    console.log("Orchestrator: shutdown complete");
    process.exit(0);
  });
});
