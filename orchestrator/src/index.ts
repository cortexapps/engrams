import { Hono } from "hono";
import { logger as honoLogger } from "hono/logger";
import { createNodeWebSocket } from "@hono/node-ws";
import { config } from "./config.ts";
import { log } from "./log.ts";
import { buildServer } from "./server.ts";
import health from "./routes/health.ts";
import authRoute from "./routes/auth.ts";
import eventsRoute from "./routes/events.ts";
import artifactsRoute from "./routes/artifacts.ts";
import portsRoute from "./routes/ports.ts";
import connectorLogoRoute from "./routes/connector-logo.ts";
import meRoute from "./routes/me.ts";
import adminRoute from "./routes/admin.ts";
import integrationOpRoute from "./routes/integration-op.ts";
import integrationOauthRoute from "./routes/integration-oauth.ts";
import slackEventsRoute from "./routes/slack-events.ts";
import slackInteractivityRoute from "./routes/slack-interactivity.ts";
// Side-effect import: registers the Slack adapter on the generic SDK seam.
import "./integrations/slack.ts";
import { makeShellRoute } from "./routes/shell.ts";
import { makePreviewProxyMiddleware } from "./routes/preview-proxy.ts";
import { registerPassthrough } from "./rpc/passthrough.ts";
import { makeDisableImageGuard } from "./rpc/image-guard.ts";
import { registerTasks } from "./rpc/tasks.ts";
import { registerProfiles } from "./rpc/profiles.ts";
import { registerMountCatalog } from "./rpc/mount-catalog.ts";
import { registerOrgSecret } from "./rpc/org-secret.ts";
import { registerMint } from "./rpc/mint.ts";
import { registerIntegration } from "./rpc/integration.ts";
import { SURFACE } from "./rpc/surface.ts";
import { controlPlaneTransport } from "./control-plane/transport.ts";
import type { ConnectRouter } from "@connectrpc/connect";
// ADR 0060: embedded DBOS engine. Workflow modules (P1+) must be imported
// ABOVE the initDbos() call below so their workflows/steps are registered
// before DBOS.launch(). Importing slack-thread.ts registers both the thread
// workflow and (transitively) the per-session ingest pump.
import { initDbos, shutdownDbos } from "./workflows/dbos.ts";
import { setThreadPolicy, setThreadControlPlane } from "./workflows/slack-thread.ts";
import { makeSlackPolicy } from "./integrations/slack-policy.ts";
import { makeThreadControlPlane } from "./workflows/thread-control-plane.ts";

const app = new Hono();

// ADR 0051 Task 21: WebSocket shell route via @hono/node-ws.
// createNodeWebSocket must be called with the Hono app BEFORE routes are mounted
// so injectWebSocket can install the upgrade handler on the node:http server.
const { upgradeWebSocket, injectWebSocket, wss } = createNodeWebSocket({ app });

// Echo 'tty' subprotocol — browsers hard-fail without this.
// handleProtocols is read by ws's handleUpgrade on each connection.
wss.options.handleProtocols = (protocols: Set<string>) =>
  protocols.has("tty") ? "tty" : false;

// Request logging (hono/logger) routed through pino, so every HTTP leg — the
// Slack webhooks included — logs `<-- METHOD path` / `--> METHOD path status ms`.
const httpLog = log.child({ component: "http" });
app.use(honoLogger((message) => httpLog.info(message)));

// ADR 0064 P2b: live-host preview reverse-proxy. Mounted FIRST so a request to
// `<slug>.<previewBaseDomain>` is resolved + tunneled to the guest port before
// the normal app routes see it; non-preview hosts fall straight through.
app.use(makePreviewProxyMiddleware());

// Mount routes.
app.route("/", health);
app.route("/", authRoute);
// ADR 0051 Task 20: browser-native HTTP legs (SSE events, artifact bytes, /me/harness-env).
app.route("/", eventsRoute);
app.route("/", artifactsRoute);
// ADR 0064 P2a: live-host port-exposure registry (CRUD). The edge reverse-proxy
// that serves the minted slugs lands in P2b.
app.route("/", portsRoute);
// Connector logos (redesign): orchestrator-owned brand marks, served for <img>.
app.route("/", connectorLogoRoute);
app.route("/", meRoute);
// ADR 0051 Task 28: admin REST proxy (pause/resume session — no gRPC equiv yet).
app.route("/", adminRoute);
// Admin trigger for the IntegrationOp seam (sessionless integration calls).
app.route("/", integrationOpRoute);
// OAuth acquisition for connectors with an `oauth` facet (e.g. Slack "Add to Slack").
app.route("/", integrationOauthRoute);
// ADR 0060: Slack external-trigger webhooks (events + interactivity). Both
// verify every request with the SDK against the slack.signing_secret org secret.
app.route("/", slackEventsRoute);
app.route("/", slackInteractivityRoute);

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

    // Native MountCatalogService (ADR 0055 P2): admin-gated + owner-stamped
    // wrapper over the coordinator's skill catalog. Registered before the
    // passthrough so it owns the MountCatalogService prefix.
    registerMountCatalog(router);

    // Native OrgSecretService (ADR 0057 A1): admin-gated proxy over the
    // coordinator's KEK-sealed org secret store. Registered before the
    // passthrough so it owns the OrgSecretService prefix; values are sealed
    // coordinator-side and never returned.
    registerOrgSecret(router);

    // Native MintService (ADR 0057 C3): admin-gated proxy over the coordinator's
    // read-only mint-kind registry (Plane-A form metadata). Before passthrough.
    registerMint(router);

    // Native IntegrationService (ADR 0057 C3): admin-gated connector catalog CRUD
    // (Plane B) over the orchestrator's own DB; built-ins are read-only seeds.
    registerIntegration(router);

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

// ADR 0060: inject the SlackThreadWorkflow's seams (the Slack provider
// mechanics + the session-lifecycle control plane) before launching the engine,
// so the first webhook-driven workflow has them. Then launch the embedded DBOS
// engine before serving any traffic, so a webhook that arrives the instant we
// bind can start a workflow.
setThreadPolicy(makeSlackPolicy());
setThreadControlPlane(makeThreadControlPlane());
await initDbos();

server.listen(config.port, "0.0.0.0", () => {
  log.info({ port: config.port }, "orchestrator listening");
});

// Graceful shutdown on SIGTERM (e.g. Tilt stop, Kubernetes pod termination).
process.on("SIGTERM", () => {
  log.info("orchestrator: SIGTERM received, shutting down gracefully");
  server.close(async (err) => {
    // Quiesce DBOS (stops queue/recovery loops, closes the system-DB pool)
    // after the HTTP server stops accepting connections.
    await shutdownDbos();
    if (err) {
      log.error({ err }, "orchestrator: error during shutdown");
      process.exit(1);
    }
    log.info("orchestrator: shutdown complete");
    process.exit(0);
  });
});
