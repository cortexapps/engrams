/**
 * Connect clients against the orchestrator's /rpc surface.
 *
 * Connect protocol over HTTP/1.1 on purpose: it traverses every hop this CLI
 * meets (vite/nginx proxies, kubectl port-forwards, the GCLB) — end-to-end
 * gRPC h2 trailers would not. Auth rides an x-api-key header injected by an
 * interceptor; junk/absent keys surface as Code.Unauthenticated from the
 * orchestrator's seams.
 */

import { createClient, type Client, type Interceptor } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import type { DescService } from "@bufbuild/protobuf";

import { SessionService } from "./gen/engram/app/v1/session_pb.ts";
import { FleetService } from "./gen/engram/app/v1/fleet_pb.ts";
import { ImageService } from "./gen/engram/app/v1/image_pb.ts";
import { TaskService } from "./gen/engram/app/v1/task_pb.ts";
import { ProfileService } from "./gen/engram/app/v1/profile_pb.ts";
import { ApiKeyService } from "./gen/engram/app/v1/api_key_pb.ts";
import { HarnessCatalogService } from "./gen/engram/app/v1/harness_pb.ts";
import { resolveApiKey } from "./config.ts";
import { fail } from "./output.ts";

/** Auth header shapes the orchestrator accepts (see extractApiKey). */
export type AuthHeader = { apiKey: string } | { bearer: string } | undefined;

function authInterceptor(auth: AuthHeader): Interceptor {
  return (next) => (req) => {
    if (auth && "apiKey" in auth) req.header.set("x-api-key", auth.apiKey);
    if (auth && "bearer" in auth) req.header.set("authorization", `Bearer ${auth.bearer}`);
    return next(req);
  };
}

export function makeClient<S extends DescService>(
  service: S,
  host: string,
  auth: AuthHeader,
): Client<S> {
  const transport = createConnectTransport({
    baseUrl: `${host}/rpc`,
    httpVersion: "1.1",
    interceptors: [authInterceptor(auth)],
  });
  return createClient(service, transport);
}

/** The lazily-built service clients for one (host, key) pair. */
export interface Clients {
  host: string;
  auth: AuthHeader;
  session: Client<typeof SessionService>;
  fleet: Client<typeof FleetService>;
  image: Client<typeof ImageService>;
  task: Client<typeof TaskService>;
  profile: Client<typeof ProfileService>;
  apiKey: Client<typeof ApiKeyService>;
  harness: Client<typeof HarnessCatalogService>;
}

/**
 * Clients for an authenticated command. Exits with the `auth login` pointer
 * when no key is resolvable — every RPC verb calls through here, so the
 * logged-out failure mode is one consistent message, not a per-command 401.
 */
export function requireClients(host: string): Clients {
  const key = resolveApiKey(host);
  if (!key) {
    fail(
      `not logged in to ${host} — run \`engrams auth login\` (or set ENGRAMS_API_KEY)`,
    );
  }
  return clientsWith(host, { apiKey: key });
}

export function clientsWith(host: string, auth: AuthHeader): Clients {
  return {
    host,
    auth,
    session: makeClient(SessionService, host, auth),
    fleet: makeClient(FleetService, host, auth),
    image: makeClient(ImageService, host, auth),
    task: makeClient(TaskService, host, auth),
    profile: makeClient(ProfileService, host, auth),
    apiKey: makeClient(ApiKeyService, host, auth),
    harness: makeClient(HarnessCatalogService, host, auth),
  };
}

/** Plain-fetch headers for the non-RPC routes (SSE events, /api/auth). */
export function authHeaders(auth: AuthHeader): Record<string, string> {
  if (auth && "apiKey" in auth) return { "x-api-key": auth.apiKey };
  if (auth && "bearer" in auth) return { authorization: `Bearer ${auth.bearer}` };
  return {};
}
