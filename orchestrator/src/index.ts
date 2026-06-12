import { Hono } from "hono";
import { config } from "./config.ts";
import { buildServer } from "./server.ts";
import health from "./routes/health.ts";
import authRoute from "./routes/auth.ts";

const app = new Hono();

// Mount routes.
app.route("/", health);
app.route("/", authRoute);

// Default 404 for unmatched Hono paths.
app.notFound((c) => c.json({ error: "not found" }, 404));

const server = buildServer(app);

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
