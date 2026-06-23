// Render-with-providers helper for component tests. Each test gets its own
// QueryClient so React-Query cache state doesn't leak across tests.
//
// Components under test call TanStack Router's <Link> / useParams / useRouterState,
// which require a RouterProvider context. We build a throwaway memory-history
// router whose root route renders the test `ui`, plus a splat child so any
// <Link to="..."> the component renders resolves without erroring. The router
// context carries `auth` (mirrors src/router.tsx's RouterContext) seeded from a
// test principal, bypassing the /me query.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRouteWithContext,
  createRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { render, type RenderOptions } from "@testing-library/react";
import { type ReactElement } from "react";
import { TransportProvider } from "@connectrpc/connect-query";
import { createRouterTransport } from "@connectrpc/connect";
import type { Transport } from "@connectrpc/connect";
import { TaskService } from "./gen/engram/app/v1/task_pb";
import { ImageService } from "./gen/engram/app/v1/image_pb";
import { SessionService } from "./gen/engram/app/v1/session_pb";
import { FleetService } from "./gen/engram/app/v1/fleet_pb";
import { AuthContextProvider, type AuthState } from "./auth/AuthProvider";
import { abilityFor } from "./lib/ability";
import type { Principal } from "./lib/types";

interface TestRouterContext {
  auth: AuthState;
}

/** Default test principal: a local admin with a saved token, matching the
 * dev synthetic admin. Override via `renderWithProviders({ principal })`. */
const DEFAULT_PRINCIPAL: Principal = {
  email: "dev@engram.local",
  display_name: "Local Admin",
  role: "admin",
  is_admin: true,
  has_claude_token: true,
  can_sign_out: true,
};

/** ADR 0051 Task 24: default in-process transport that stubs TaskService,
 * ImageService, and SessionService with empty/success responses. Tests that
 * exercise specific RPC behaviour should supply their own transport via the
 * `transport` option so they can control the response. */
export const testTransport: Transport = createRouterTransport((router) => {
  router.service(TaskService, {
    listTasks: () => ({ tasks: [] }),
    createTask: () => ({ task: undefined }),
    getTask: () => ({ task: undefined }),
    deleteTask: () => ({}),
  });
  router.service(ImageService, {
    listEnabledImages: () => ({ images: [] }),
    enableImage: () => ({ job: undefined }),
    disableImage: () => ({}),
    refreshImage: () => ({ job: undefined }),
    listEnableJobs: () => ({ jobs: [] }),
    getEnableJob: () => ({ job: undefined }),
    retryEnableJob: () => ({ job: undefined }),
    listRegistries: () => ({ registries: [] }),
    addRegistry: () => ({ id: "", host: "", authKind: "", authPrincipal: undefined }),
    deleteRegistry: () => ({}),
  });
  router.service(FleetService, {
    listHosts: () => ({ hosts: [] }),
    getHost: () => ({ host: undefined }),
    getHostCowState: () => ({ hostId: "", sessions: [] }),
    drainHost: () => ({}),
    adminDrainHost: () => ({ hostId: "", evacuating: [], failures: [] }),
    cordonHost: () => ({ hostId: "", status: "" }),
    uncordonHost: () => ({ hostId: "", status: "" }),
    getStorageSummary: () => ({
      snapshots: 0n,
      snapshotBytes: 0n,
      gcPending: 0n,
      trackedSandboxes: 0n,
      dirtyChunks: 0n,
      unflushedBytes: 0n,
      avgLocalityPct: 0,
      rows: [],
    }),
    flushSession: () => ({ outcome: "", manifestVersion: undefined }),
    evacuateSession: () => ({ sessionId: "", status: "" }),
    chunkGc: () => ({
      listedChunks: 0n,
      malformedKeys: 0n,
      pinSetSize: 0n,
      candidatesMarked: 0n,
      restartCount: 0,
      restartBudgetExhausted: false,
      promotedDeletes: 0n,
      promoteDeleteErrors: 0n,
      graceSecs: 0n,
    }),
    bundleGc: () => ({
      listed: 0n,
      pinSetSize: 0n,
      candidatesMarked: 0n,
      promotedDeletes: 0n,
      promoteDeleteErrors: 0n,
      restartCount: 0,
    }),
    snapshotBlobGc: () => ({
      listed: 0n,
      malformed: 0n,
      pinSetSize: 0n,
      candidatesMarked: 0n,
      promotedDeletes: 0n,
      promoteRepinnedSkips: 0n,
      promoteDeleteErrors: 0n,
      restartCount: 0,
    }),
  });
  // ADR 0051 Task 24: stub SessionService so components that call
  // getSession / getCowState / listCheckpoints / sendPrompt / interrupt
  // (including SessionThread's useMutation initialisation) don't error in tests.
  router.service(SessionService, {
    listSessions: () => ({ sessions: [] }),
    createSession: () => ({
      sessionId: "",
      status: "",
      imageVersion: "",
      kind: "",
    }),
    getSession: () => ({ session: undefined }),
    deleteSession: () => ({}),
    sendPrompt: () => ({ sessionId: "", note: "" }),
    interrupt: () => ({ sessionId: "", note: "" }),
    answerQuestion: () => ({ sessionId: "", note: "" }),
    getLog: () => ({ sessionId: "", kind: "", events: [] }),
    snapshot: () => ({ sessionId: "", snapshotId: undefined, sizeBytes: undefined, note: "" }),
    resume: () => ({ sessionId: "", snapshotId: undefined, sizeBytes: undefined, note: "" }),
    evictLocal: () => ({}),
    getCowState: () => ({ sessionId: "", state: undefined }),
    listCheckpoints: () => ({ sessionId: "", checkpoints: [] }),
    createArtifactFromPath: () => ({ artifactId: "", mediaType: "", sizeBytes: BigInt(0) }),
  });
});

export interface RenderWithProvidersOptions extends Omit<RenderOptions, "wrapper"> {
  /** Reuse a caller-supplied client (rare — for multi-step tests that need
   * cache continuity). Default: a fresh client per call. */
  queryClient?: QueryClient;
  /** Principal injected into the auth context (bypasses the /me query so tests
   * don't each need a fetch mock). Defaults to a local admin. */
  principal?: Principal;
  /** Connect transport to use. Defaults to `testTransport` (stubs all RPCs
   * with empty success responses). Pass a custom transport for tests that
   * need to control TaskService/ImageService responses. */
  transport?: Transport;
}

export function renderWithProviders(
  ui: ReactElement,
  {
    queryClient,
    principal = DEFAULT_PRINCIPAL,
    transport,
    ...renderOptions
  }: RenderWithProvidersOptions = {},
) {
  const client =
    queryClient ??
    new QueryClient({
      defaultOptions: {
        queries: { retry: false, gcTime: 0, staleTime: 0 },
        mutations: { retry: false },
      },
    });

  const authValue: AuthState = {
    principal,
    isAdmin: principal.is_admin,
    ability: abilityFor({ id: "test-user-id", role: principal.is_admin ? "admin" : "user" }),
    refresh: () => {},
  };

  const resolvedTransport = transport ?? testTransport;

  const rootRoute = createRootRouteWithContext<TestRouterContext>()({
    // Wrap in TransportProvider + QueryClientProvider + AuthContextProvider so
    // components that use connect-query hooks or auth contexts work.
    component: () => (
      <TransportProvider transport={resolvedTransport}>
        <QueryClientProvider client={client}>
          <AuthContextProvider value={authValue}>{ui}</AuthContextProvider>
        </QueryClientProvider>
      </TransportProvider>
    ),
  });

  // Splat child: any <Link to="..."> resolves to a real match during
  // buildLocation, so rendering links to app paths we aren't exercising
  // doesn't throw.
  const splatRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "$",
    component: () => null,
  });

  const router = createRouter({
    routeTree: rootRoute.addChildren([splatRoute]),
    history: createMemoryHistory({ initialEntries: ["/"] }),
    context: { auth: authValue },
  });

  return {
    ...render(<RouterProvider router={router} />, renderOptions),
    queryClient: client,
  };
}
