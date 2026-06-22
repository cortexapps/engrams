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
import { OrgSecretService } from "../gen/engram/app/v1/org_secret_pb.ts";

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

/** OrgSecretService client — admin-managed, KEK-sealed org secret store (ADR 0057). */
export const orgSecret = createClient(OrgSecretService, controlPlaneTransport);
