import { Hono } from "hono";
import { config } from "./config.ts";
import { buildServer } from "./server.ts";
import health from "./routes/health.ts";
import authRoute from "./routes/auth.ts";
import eventsRoute from "./routes/events.ts";
import artifactsRoute from "./routes/artifacts.ts";
import meRoute from "./routes/me.ts";
import { registerPassthrough } from "./rpc/passthrough.ts";
import { registerTasks } from "./rpc/tasks.ts";
import { SURFACE } from "./rpc/surface.ts";
import { controlPlaneTransport } from "./control-plane/transport.ts";
import type { ConnectRouter } from "@connectrpc/connect";

const app = new Hono();

// Mount routes.
app.route("/", health);
app.route("/", authRoute);
// ADR 0039 Task 20: browser-native HTTP legs (SSE events, artifact bytes, /me/claude-token).
app.route("/", eventsRoute);
app.route("/", artifactsRoute);
app.route("/", meRoute);

// Default 404 for unmatched Hono paths.
app.notFound((c) => c.json({ error: "not found" }, 404));

const server = buildServer(app, (router: ConnectRouter) => {
  // Native TaskService: orchestrator-owned task model (ADR 0039 §3, Task 19).
  // Registered BEFORE the passthrough so it wins the /rpc/engram.app.v1.TaskService/* prefix.
  registerTasks(router);

  // Generic passthrough: forwards SessionService, FleetService, ImageService
  // to the control plane with per-method CASL authz gate (ADR 0039 Task 18).
  registerPassthrough(router, SURFACE, controlPlaneTransport);
});

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
