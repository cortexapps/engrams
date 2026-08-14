import { randomUUID } from "node:crypto";
import { Hono } from "hono";
import { logger as honoLogger } from "hono/logger";
import { createNodeWebSocket } from "@hono/node-ws";
import { config } from "./config.ts";
import { log } from "./log.ts";
import { buildServer } from "./server.ts";
import health from "./routes/health.ts";
import authRoute from "./routes/auth.ts";
import authConfigRoute from "./routes/auth-config.ts";
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
import githubEventsRoute from "./routes/github-events.ts";
import hooksRoute from "./routes/hooks.ts";
import reviewsDispatchRoute from "./routes/reviews-dispatch.ts";
import { makeGoogleOidcRoute } from "./routes/google-oidc.ts";
import { makeConnectionCredentialBrokerRoute } from "./routes/connection-credential-broker.ts";
import { makeOidcKeyAdminRoute } from "./routes/oidc-key-admin.ts";
import { startOidcKeyRotation } from "./integrations/oidc-key-rotation.ts";
import { makeIntegrationOidcKeyStore } from "./db/integration-oidc-keys.ts";
// Importing registers the Slack adapter on the generic SDK seam.
import "./integrations/slack.ts";
import { makeShellRoute } from "./routes/shell.ts";
import { makeVncRoute } from "./routes/vnc.ts";
import { makeIdeRoute, makeIdeUpgradeHandler } from "./routes/ide.ts";
import { makeSpecSyncUpgradeHandler, SpecSyncHub } from "./routes/spec-sync.ts";
import { makePreviewProxyMiddleware } from "./routes/preview-proxy.ts";
import { makePreviewUpgradeHandler } from "./routes/preview-ws.ts";
import { registerPassthrough } from "./rpc/passthrough.ts";
import { makeDisableImageGuard } from "./rpc/image-guard.ts";
import { makeExternalToolCompletionGuard } from "./rpc/tool-completion-guard.ts";
import { registerBuiltinTools } from "./tools/builtin.ts";
import { registerSpecTools } from "./tools/specs.ts";
import { registerReviewTools } from "./tools/review.ts";
import { registerDevTools } from "./tools/dev-tools.ts";
import { registerTasks } from "./rpc/tasks.ts";
import { registerProfiles } from "./rpc/profiles.ts";
import { registerPapercuts } from "./rpc/papercuts.ts";
import { registerPrRefs } from "./rpc/pr-refs.ts";
import { registerReviews } from "./rpc/reviews.ts";
import { registerMountCatalog } from "./rpc/mount-catalog.ts";
import { registerOrgSecret } from "./rpc/org-secret.ts";
import { registerMint } from "./rpc/mint.ts";
import { registerApiKeys } from "./rpc/api-key.ts";
import { registerIntegration } from "./rpc/integration.ts";
import { registerArtifacts } from "./rpc/artifacts.ts";
import { registerSpecs } from "./rpc/specs.ts";
import { registerAutomations } from "./rpc/automations.ts";
import { SURFACE } from "./rpc/surface.ts";
import { controlPlaneTransport } from "./control-plane/transport.ts";
import {
  harnessCatalog as controlPlaneHarnessCatalog,
  images as controlPlaneImages,
  sessions as controlPlaneSessions,
} from "./control-plane/client.ts";
import type { ConnectRouter } from "@connectrpc/connect";
// ADR 0060: embedded DBOS engine. Workflow modules (P1+) must be imported
// ABOVE the initDbos() call below so their workflows/steps are registered
// before DBOS.launch(). Imports below register the finite Slack-thread and
// tool-execution workflows.
import { initDbos, shutdownDbos } from "./workflows/dbos.ts";
import { setThreadPolicy, setThreadControlPlane } from "./workflows/slack-thread.ts";
import { setReviewControlPlane } from "./workflows/pr-review.ts";
import { setReviewIngressControlPlane } from "./workflows/review-ingress.ts";
import { startSpecTicketSyncWorkflow } from "./workflows/spec-ticket-sync.ts";
import { makeSlackPolicy } from "./integrations/slack-policy.ts";
import { makeThreadControlPlane } from "./workflows/thread-control-plane.ts";
import { makeReviewControlPlane } from "./workflows/review-control-plane.ts";
import { makeProductionListenerManager } from "./listeners/manager.ts";
import { makeProductionAutomationScheduler } from "./automations/scheduler.ts";
import { assertSweepPoliciesExhaustive } from "./sweep/policy.ts";
import { makeSweepRuntime } from "./sweep/production.ts";
import { getDb, getPool } from "./db/client.ts";
import { resolveDraftSpec, resolveSpecMembership } from "./authz/resolve.ts";
import { makePapercutStore } from "./db/papercuts.ts";
import { makeReviewStore } from "./db/reviews.ts";
import { makeReviewTargetHydrationStore } from "./db/review-target-hydration.ts";
import { makeEnrollmentStore } from "./db/enrollments.ts";
import {
  PostgresSpecDocumentStore,
  proseMirrorDocument,
  SpecDocumentService,
} from "./specs/doc-service.ts";
import { PostgresSpecAwarenessBus, PostgresSpecParticipantStore } from "./specs/sync-store.ts";
import { setSpecPresence, specPresence } from "./specs/presence.ts";
import { PostgresOpenQuestionStore, OpenQuestionService } from "./specs/open-questions.ts";
import { SpecQuestionDocument } from "./specs/question-document.ts";
import { PostgresSectionStateStore, SectionStateService } from "./specs/section-state-service.ts";
import { PostgresSpecToolMetadataStore, SpecToolService } from "./specs/tool-service.ts";
import {
  PostgresPinnedSpecReader,
  PostgresSpecTicketStore,
  SpecTicketTreeService,
} from "./specs/ticket-tree.ts";
import { makeProfileStore } from "./db/profiles.ts";
import { makeIntegrationConnectionStore } from "./db/integration-connections.ts";
import { makeConnectorStore } from "./db/connectors.ts";
import { loadRegistry } from "./connectors/registry.ts";
import { tools } from "./tools/registry.ts";
import { renderReviewer } from "./reviewers/render.ts";
import { makeSessionFilesRoute } from "./routes/session-files.ts";
import { makeSpecsRoute, PostgresSpecReadStore } from "./routes/specs.ts";
import { makeSpecRailRoute, PostgresSpecRailStore } from "./routes/spec-rail.ts";
import { makeSpecBlockIterationRoute } from "./routes/spec-block-iteration.ts";
import { makeSpecMessagesRoute, PostgresSpecMessageStore } from "./routes/spec-messages.ts";
import { makeSpecEventsRoute } from "./routes/spec-events.ts";
import { makeSpecTemplatesRoute } from "./routes/spec-templates.ts";
import { makeSpecTemplateCatalog } from "./specs/template-catalog.ts";
import { createSpec, makeSpecCreateStore } from "./specs/create.ts";
import { createTaskWithSession } from "./rpc/task-create.ts";
import { makeUserSecretStore } from "./db/user-secrets.ts";
import { productionSpecProjection } from "./specs/projection.ts";
import { PostgresSpecCheckpointStore, SpecCheckpointService } from "./specs/checkpoints.ts";
import { GapCheckService, PostgresGapCheckStore } from "./specs/gap-check.ts";
import { PostgresSpecPublishStore, SpecPublishService } from "./specs/publish.ts";
import {
  productionSpecPublishArtifactPublisher,
  productionSpecTicketizeHandoff,
} from "./specs/publish-artifact.ts";
import {
  DEFAULT_SPEC_PUBLISH_SCANNER_CONFIG,
  SpecPublishScanner,
} from "./specs/publish-scanner.ts";
import { makeSpecPublishRoute } from "./routes/spec-publish.ts";
import { makeSpecTicketRoute } from "./routes/spec-tickets.ts";
import { makeSpecTicketSyncRoute } from "./routes/spec-ticket-sync.ts";
import { makeLinearIssueClient } from "./integrations/linear-issues.ts";
import { SpecTicketSyncService } from "./specs/ticket-sync-service.ts";
import { PostgresSpecTicketSyncStore } from "./specs/ticket-sync-store.ts";
import { makeSpecTicketSyncConnector } from "./specs/ticket-sync-connector.ts";
import { seedReviewerProfile } from "./reviewers/seed-profile.ts";
import { makeGithubReviewPoster } from "./reviews/github-review.ts";
import { DEFAULT_TARGET_HYDRATOR_CONFIG, TargetHydrator } from "./reviews/target-hydrator.ts";

const app = new Hono();
const warnSpecDocument = (message: string) => log.warn({ message }, "spec document warning");
const specNow = () => new Date();
const specDocuments = new SpecDocumentService(
  new PostgresSpecDocumentStore(getPool(), { onWarning: warnSpecDocument }),
  { onWarning: warnSpecDocument, now: specNow },
);
const specOpenQuestions = new PostgresOpenQuestionStore(getPool());
const specSectionStates = new SectionStateService({
  store: new PostgresSectionStateStore(getPool()),
  now: specNow,
});
// The post-publish ticket tree (ADR 0114 D6). It reads the pinned checkpoint,
// never the live head, so every §backlink stays resolvable.
const specLinear = makeLinearIssueClient();
const specTickets = new SpecTicketTreeService({
  store: new PostgresSpecTicketStore(getPool()),
  pinned: new PostgresPinnedSpecReader(getPool()),
  newId: () => randomUUID(),
});
// Linear sync (ADR 0114 D6, N4). The route records the intent; a DBOS workflow
// creates the issues, one durable step per ticket, so a pod roll resumes the
// batch and the ledger keeps a retry from creating a second issue.
const specTicketSync = new SpecTicketSyncService({
  store: new PostgresSpecTicketSyncStore(getPool()),
  linear: specLinear,
  connector: makeSpecTicketSyncConnector(),
  log: log.child({ component: "spec-ticket-sync" }),
  start: startSpecTicketSyncWorkflow,
});
const specToolService = new SpecToolService({
  documents: specDocuments,
  sectionStates: specSectionStates,
  questions: new OpenQuestionService({
    store: specOpenQuestions,
    document: new SpecQuestionDocument(specDocuments, "spec-agent-question"),
    now: specNow,
  }),
  questionStore: specOpenQuestions,
  metadata: new PostgresSpecToolMetadataStore(getPool(), specNow),
  tickets: specTickets,
  now: specNow,
});
const specRailStore = new PostgresSpecRailStore(getPool());
const specMessageStore = new PostgresSpecMessageStore(getPool());
const specGapCheck = new GapCheckService({
  documents: specDocuments,
  railStore: specRailStore,
  store: new PostgresGapCheckStore(getPool()),
  toolDocuments: specToolService,
  now: specNow,
});
const specParticipants = new PostgresSpecParticipantStore(getDb());
const specCheckpointStore = new PostgresSpecCheckpointStore(getPool());
const specCheckpoints = new SpecCheckpointService(specDocuments, specCheckpointStore);
// The publish gate (ADR 0114 D10). The route records the intent; the scanner
// pins the checkpoint, records the artifact version, and starts ticketize.
const specPublishStore = new PostgresSpecPublishStore(getPool());
const specPublish = new SpecPublishService({
  store: specPublishStore,
  railStore: specRailStore,
  documents: specDocuments,
  gapCheck: specGapCheck,
  now: specNow,
});
const specPublishScanner = new SpecPublishScanner({
  store: specPublishStore,
  documents: specDocuments,
  gate: specPublish,
  checkpointStore: specCheckpointStore,
  artifacts: productionSpecPublishArtifactPublisher(),
  ticketize: productionSpecTicketizeHandoff(),
  config: DEFAULT_SPEC_PUBLISH_SCANNER_CONFIG,
  now: specNow,
  log: log.child({ component: "spec-publish-scanner" }),
});
const warnSpecSync = (message: string) => log.warn({ message }, "spec sync warning");
const specAwarenessBus = new PostgresSpecAwarenessBus(getPool(), { onWarning: warnSpecSync });
const specSyncHub = new SpecSyncHub({
  documents: specDocuments,
  participants: specParticipants,
  awarenessBus: specAwarenessBus,
  onWarning: warnSpecSync,
});
setSpecPresence(specSyncHub);

// ADR 0051 Task 21: WebSocket shell route via @hono/node-ws.
// createNodeWebSocket must be called with the Hono app BEFORE routes are mounted
// so injectWebSocket can install the upgrade handler on the node:http server.
const { upgradeWebSocket, injectWebSocket, wss } = createNodeWebSocket({ app });

// Subprotocol selection, read by ws's handleUpgrade on each connection.
// The /shell client offers 'tty' (xterm.js hard-fails without it echoed back).
// The /vnc noVNC client offers no subprotocol (modern) or 'binary' (older), so
// echo 'binary' when offered and otherwise select none (return false → no
// subprotocol selected, the upgrade still proceeds per RFC 6455). The 'tty'
// branch stays first so the shell path is unaffected.
wss.options.handleProtocols = (protocols: Set<string>) =>
  protocols.has("tty") ? "tty" : protocols.has("binary") ? "binary" : false;

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
// Public auth posture for the SPA login page (which doors are open). Sits
// alongside the better-auth mount; unauthenticated by design (pre-login).
app.route("/", authConfigRoute);
// ADR 0109: public OIDC metadata for customer WIF providers. No session or
// credential data is returned from these endpoints.
app.route("/", makeGoogleOidcRoute());
// Host-only broker endpoint. The broker's own bearer authenticates callers
// (CONNECTION_BROKER_BEARER; the coordinator sends
// ENGRAM_CONNECTION_BROKER_BEARER).
app.route("/", makeConnectionCredentialBrokerRoute());
// Explicit admin trigger for OIDC signing-key rotation (ADR 0109).
app.route("/", makeOidcKeyAdminRoute());
// ADR 0051 Task 20: browser-native HTTP legs (SSE events, artifact bytes, /me/harness-env).
app.route("/", eventsRoute);
app.route("/", artifactsRoute);
app.route("/", makeSessionFilesRoute());
app.route(
  "/",
  makeSpecsRoute({
    store: new PostgresSpecReadStore(getPool()),
    checkpointStore: specCheckpointStore,
    checkpoints: specCheckpoints,
    resolveMembership: resolveSpecMembership,
    orgId: config.deploymentId,
    create: (request) =>
      createSpec(
        {
          store: makeSpecCreateStore(getDb()),
          catalog: makeSpecTemplateCatalog(),
          documents: specDocuments,
          // R5: a spec session is an ordinary session on the chosen profile, so
          // it rides the one create primitive. The task type "spec" is what
          // selects the spec tool manifest and the spec-mode system prompt.
          startSession: async (input) => {
            await createTaskWithSession(
              {
                profiles: makeProfileStore(getDb()),
                images: controlPlaneImages,
                connectors: { list: () => makeConnectorStore(getDb()).list() },
                harnessCatalog: controlPlaneHarnessCatalog,
                sessions: controlPlaneSessions,
                secrets: makeUserSecretStore(getDb()),
                db: getDb(),
                newTaskId: () => input.taskId,
                newSessionId: () => input.sessionId,
              },
              {
                type: "spec",
                ownerUserId: input.ownerUserId,
                ...(input.ownerIsServiceAccount ? { ownerIsServiceAccount: true } : {}),
                profileId: input.profileId,
                title: input.title,
                prompt: input.prompt,
                specTemplate: input.specTemplate,
              },
            );
          },
        },
        request,
      ),
  }),
);
app.route(
  "/",
  makeSpecRailRoute({
    store: specRailStore,
    documents: specDocuments,
    sectionStates: specSectionStates,
    resolveMembership: resolveSpecMembership,
  }),
);
app.route(
  "/",
  makeSpecPublishRoute({
    publish: specPublish,
    wake: (specId) => specPublishScanner.wake(specId),
    resolveMembership: resolveSpecMembership,
  }),
);
app.route(
  "/",
  makeSpecTicketRoute({
    tickets: specTickets,
    resolveMembership: resolveSpecMembership,
  }),
);
app.route(
  "/",
  makeSpecTicketSyncRoute({
    sync: specTicketSync,
    resolveMembership: resolveSpecMembership,
    readWorkspace: () => specLinear.readWorkspace(),
  }),
);
app.route(
  "/",
  makeSpecBlockIterationRoute({
    resolveMembership: resolveSpecMembership,
    resolveTarget: async (specId) => {
      const result = await getPool().query<{ session_id: string | null }>(
        "SELECT session_id FROM spec WHERE id = $1",
        [specId],
      );
      const sessionId = result.rows[0]?.session_id;
      if (!sessionId) return null;
      const loaded = await specDocuments.syncFromLog(specId);
      return { sessionId, document: proseMirrorDocument(loaded.doc) };
    },
    preparePrompt: (sessionId, status) => productionSpecProjection.preparePrompt(sessionId, status),
  }),
);
app.route(
  "/",
  makeSpecMessagesRoute({
    store: specMessageStore,
    resolveMembership: resolveSpecMembership,
    preparePrompt: (sessionId, status) => productionSpecProjection.preparePrompt(sessionId, status),
  }),
);
app.route(
  "/",
  makeSpecEventsRoute({
    resolveMembership: resolveSpecMembership,
    resolveSessionId: (specId) => specMessageStore.resolveSessionId(specId),
  }),
);
app.route(
  "/",
  makeSpecTemplatesRoute({
    catalog: makeSpecTemplateCatalog(),
    orgId: config.deploymentId,
  }),
);
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
// ADR 0100: GitHub's signed webhook and the bearer-authenticated CI trigger
// converge on the same durable per-PR workflow.
app.route("/", githubEventsRoute);
// ADR 0102: dynamically registered webhooks verify their own registration
// secret and dispatch one-shot AutomationRunWorkflow occurrences.
app.route("/", hooksRoute);
app.route("/", reviewsDispatchRoute);

// ADR 0051 Task 21: Shell WebSocket route.
const { app: shellApp, injectUpgrade } = makeShellRoute();
injectUpgrade(upgradeWebSocket);
app.route("/", shellApp);

// ADR 0065/0066: VNC WebSocket route (browser tab → noVNC → EnsureBrowser +
// raw RFB over the port relay).
const { app: vncApp, injectUpgrade: injectVncUpgrade } = makeVncRoute();
injectVncUpgrade(upgradeWebSocket);
app.route("/", vncApp);

// ADR 0085: in-guest IDE (code-server) HTTP proxy. Session-scoped + guarded;
// the WS half is the upgrade hook passed to buildServer below. Mounted after
// shell/vnc (paths are disjoint; the preview middleware above is Host-keyed
// and passes non-preview hosts straight through).
const { app: ideApp } = makeIdeRoute();
app.route("/", ideApp);

// Default 404 for unmatched Hono paths.
app.notFound((c) => c.json({ error: "not found" }, 404));

const server = buildServer(
  app,
  (router: ConnectRouter) => {
    // Native TaskService: orchestrator-owned task model (ADR 0051 §3, Task 19).
    // Registered BEFORE the passthrough so it wins the /rpc/engram.app.v1.TaskService/* prefix.
    registerTasks(router);

    // Native PapercutService: orchestrator-owned friction inbox.
    registerPapercuts(router);

    // Native PrRefService: durable task/session links to authored PRs (ADR 0100).
    registerPrRefs(router);

    // Native ReviewService: org-visible durable PR review records (ADR 0100).
    registerReviews(router);

    // Native ProfileService: orchestrator-owned session profiles (ADR 0053).
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

    // Native ApiKeyService (ADR 0086): admin-gated global API keys over the
    // orchestrator's own DB + the better-auth api-key plugin. Before
    // passthrough. The plugin's own HTTP endpoints are 404'd in better-auth.ts.
    registerApiKeys(router);

    // Native IntegrationService (ADR 0057 C3): admin-gated connector catalog CRUD
    // (Plane B) over the orchestrator's own DB; built-ins are read-only seeds.
    registerIntegration(router);

    // Native AutomationService + WebhookRegistrationService (ADR 0102):
    // admin-authored triggers, render preview, run/sample history, and sealed
    // per-registration webhook secrets.
    registerAutomations(router);

    // Native ArtifactService: the cross-session artifact registry (owner /
    // org-shared / admin via the shared service layer). Before passthrough.
    registerArtifacts(router);

    // Native SpecService: organization-shared list from orchestrator PG only.
    registerSpecs(router);

    // Generic passthrough: forwards SessionService, FleetService, ImageService
    // to the control plane with per-method CASL authz gate (ADR 0051 Task 18).
    // ADR 0053 §3: a DisableImage pre-flight blocks disabling an image that
    // any active profile still references (the coordinator only knows sessions).
    registerPassthrough(router, SURFACE, controlPlaneTransport, undefined, undefined, {
      "ImageService.DisableImage": makeDisableImageGuard(),
      "SessionService.CompleteToolCall": makeExternalToolCompletionGuard(),
      "SessionService.SendPrompt": async (request) => {
        const sessionId = (request as { sessionId?: unknown }).sessionId;
        if (typeof sessionId !== "string") return;
        const response = await controlPlaneSessions.getSession({ sessionId });
        await productionSpecProjection.preparePrompt(sessionId, response.session?.status ?? "");
      },
    });
  },
  // Pass the full NodeWebSocket handle so buildServer can install the
  // Bun-compatible upgrade handler (wss.handleUpgrade instead of socket.end).
  // injectWebSocket is included for completeness but the custom upgrade handler
  // is used instead of calling nodeWs.injectWebSocket(server).
  { upgradeWebSocket, wss, injectWebSocket },
  // Raw WS-upgrade hooks, tried in order before the shell/vnc path: the
  // preview proxy first (Host-keyed — a preview host is a different origin,
  // so it wins outright), then the IDE proxy (path-keyed, ADR 0085).
  [
    makePreviewUpgradeHandler(),
    makeIdeUpgradeHandler(),
    makeSpecSyncUpgradeHandler(
      {
        documents: specDocuments,
        participants: specParticipants,
        awarenessBus: specAwarenessBus,
        resolveMembership: resolveSpecMembership,
        resolveDraft: resolveDraftSpec,
      },
      specSyncHub,
    ),
  ],
);

// ADR 0060: inject the SlackThreadWorkflow's seams (the Slack provider
// mechanics + the session-lifecycle control plane) before launching the engine,
// so the first webhook-driven workflow has them. Then launch the embedded DBOS
// engine before serving any traffic, so a webhook that arrives the instant we
// bind can start a workflow.
setThreadPolicy(makeSlackPolicy());
setThreadControlPlane(makeThreadControlPlane());
const reviewControlPlane = makeReviewControlPlane({
  sessions: controlPlaneSessions,
  profiles: makeProfileStore(getDb()),
  enrollments: makeEnrollmentStore(getDb()),
  renderReviewer,
});
setReviewControlPlane(reviewControlPlane);
// Review ingress shares the same control plane: it resolves the change, then
// starts the pass (ADR 0100 d11).
setReviewIngressControlPlane(reviewControlPlane);
// ADR 0089: production built-ins and optional dev smoke tools are registered
// before DBOS launches so manifest compilation and tool execution see them.
registerBuiltinTools(tools, { papercuts: makePapercutStore(getDb()) });
registerReviewTools(tools, { reviews: makeReviewStore(getDb()) });
registerSpecTools(tools, {
  resolveSpecForSession: async (sessionId) => {
    const id = await productionSpecProjection.storeSpecForSession(sessionId);
    return id ? { id } : null;
  },
  documents: specToolService,
  projection: productionSpecProjection,
  presence: specPresence,
  gapCheck: specGapCheck,
});
const integrationConnections = makeIntegrationConnectionStore(getDb());
const configuredConnectors = await loadRegistry(makeConnectorStore(getDb()));
await Promise.all([
  ...[...configuredConnectors.values()].map((connector) =>
    integrationConnections.ensureDefault(connector.provider, `${connector.display.name} (default)`),
  ),
  integrationConnections.ensureDefault("engram", "Engrams tools"),
]);
void seedReviewerProfile(makeProfileStore(getDb()), integrationConnections, log).catch((err) =>
  log.error({ err }, "reviewer profile seed failed"),
);
if (process.env.ENGRAM_DEV_TOOLS === "1") registerDevTools();
await initDbos();
assertSweepPoliciesExhaustive();
const { heartbeat, sweeper } = makeSweepRuntime({
  config: {
    sweepDisabled: config.sweepDisabled,
    sweepIntervalMs: config.sweepIntervalMs,
    sweepGraceMs: config.sweepGraceMs,
    sweepHeartbeatIntervalMs: config.sweepHeartbeatIntervalMs,
  },
});
// Always heartbeat, including under the sweeper kill switch: otherwise another
// pod can mistake this live DBOS application version for an abandoned owner.
await heartbeat.start();
if (!config.sweepDisabled) {
  await sweeper.start();
} else {
  log.warn("DBOS orphan sweep disabled via ORCHESTRATOR_SWEEP_DISABLED");
}
// Plain timer driver, not a DBOS workflow: hydration is bounded database/API
// maintenance and does not need durable workflow replay or a sweep policy.
const targetHydrator = new TargetHydrator({
  config: DEFAULT_TARGET_HYDRATOR_CONFIG,
  store: makeReviewTargetHydrationStore(getDb()),
  github: makeGithubReviewPoster(),
  now: () => new Date(),
  log: log.child({ component: "review-target-hydrator" }),
});
await targetHydrator.start();
await specDocuments.startPeerSync();
await specSyncHub.start();
await specPublishScanner.start();
const listenerManager = makeProductionListenerManager();
await listenerManager.start();
const automationScheduler = makeProductionAutomationScheduler();
await automationScheduler.start();
// ADR 0109: scheduled OIDC signing-key rotation. The first step also creates
// the active key, so the public JWKS GET stays read-only.
const oidcKeyRotation = startOidcKeyRotation({
  keys: makeIntegrationOidcKeyStore(getDb()),
});
server.listen(config.port, "0.0.0.0", () => {
  log.info({ port: config.port }, "orchestrator listening");
});

// Graceful shutdown on SIGTERM (e.g. Tilt stop, Kubernetes pod termination).
process.on("SIGTERM", () => {
  log.info("orchestrator: SIGTERM received, shutting down gracefully");
  void (async () => {
    const serverStopped = new Promise<Error | undefined>((resolve) => {
      server.close((err) => resolve(err));
    });
    // Quiesce DBOS after the HTTP server stops accepting connections.
    oidcKeyRotation.stop();
    await automationScheduler.stop();
    await listenerManager.stop();
    await specPublishScanner.stop();
    await specSyncHub.stop();
    await specDocuments.stopPeerSync();
    const serverError = await serverStopped;
    await targetHydrator.stop();
    await sweeper.stop();
    // The heartbeat must outlive the DBOS drain. Workflows can execute until
    // shutdownDbos() returns, and another pod must see this owner as live.
    await shutdownDbos();
    await heartbeat.stop();
    if (serverError) {
      log.error({ err: serverError }, "orchestrator: error during shutdown");
      process.exit(1);
    }
    log.info("orchestrator: shutdown complete");
    process.exit(0);
  })();
});
