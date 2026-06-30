/**
 * Typed control-plane clients (ADR 0051 §5).
 *
 * One singleton client per service, all backed by the shared
 * controlPlaneTransport (bearer header + H2 keepalives).
 *
 * Usage:
 *   import { sessions, shellRelay, images, fleet } from "./client.ts";
 *   const resp = await sessions.listSessions({});
 */

import { createClient } from "@connectrpc/connect";
import { controlPlaneTransport } from "./transport.ts";

import { SessionService, ShellRelayService } from "../gen/engram/app/v1/session_pb.ts";
import { ImageService } from "../gen/engram/app/v1/image_pb.ts";
import { FleetService } from "../gen/engram/app/v1/fleet_pb.ts";
import { MountCatalogService } from "../gen/engram/app/v1/mount_catalog_pb.ts";
import { HarnessCatalogService } from "../gen/engram/app/v1/harness_pb.ts";
import { OrgSecretService } from "../gen/engram/app/v1/org_secret_pb.ts";
import { MintService } from "../gen/engram/app/v1/mint_pb.ts";
import { IntegrationOpService } from "../gen/engram/app/v1/integration_op_pb.ts";

/** SessionService client — session lifecycle (create/get/delete/stream/exec). */
export const sessions = createClient(SessionService, controlPlaneTransport);

/** ShellRelayService client — bidi shell relay stream. */
export const shellRelay = createClient(ShellRelayService, controlPlaneTransport);

/** ImageService client — image prefetch/disable/list. */
export const images = createClient(ImageService, controlPlaneTransport);

/** FleetService client — host view, drain, cordon, GC. */
export const fleet = createClient(FleetService, controlPlaneTransport);

/** MountCatalogService client — org-shared user-uploaded skill catalog (ADR 0055 P2). */
export const mountCatalog = createClient(MountCatalogService, controlPlaneTransport);

/** HarnessCatalogService client — the harness catalog (ADR 0062): list + register. */
export const harnessCatalog = createClient(HarnessCatalogService, controlPlaneTransport);

/** OrgSecretService client — admin-managed, KEK-sealed org secret store (ADR 0057). */
export const orgSecret = createClient(OrgSecretService, controlPlaneTransport);

/** MintService client — read-only mint-kind registry / Plane-A form metadata (ADR 0057 C3). */
export const mint = createClient(MintService, controlPlaneTransport);

/** IntegrationOpService client — server-side, sessionless integration invocation
 * (RunIntegrationOp) + credential resolution for off-the-shelf SDKs (Mode B). */
export const integrationOp = createClient(IntegrationOpService, controlPlaneTransport);
