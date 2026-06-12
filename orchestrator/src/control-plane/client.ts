/**
 * Typed control-plane clients (ADR 0039 §5).
 *
 * One singleton client per service, all backed by the shared
 * controlPlaneTransport (bearer header + H2 keepalives).
 *
 * Usage:
 *   import { sessions, shellRelay, secrets, images, fleet } from "./client.ts";
 *   const resp = await sessions.listSessions({});
 */

import { createClient } from "@connectrpc/connect";
import { controlPlaneTransport } from "./transport.ts";

import { SessionService, ShellRelayService } from "../gen/engram/app/v1/session_pb.ts";
import { SecretService } from "../gen/engram/app/v1/secret_pb.ts";
import { ImageService } from "../gen/engram/app/v1/image_pb.ts";
import { FleetService } from "../gen/engram/app/v1/fleet_pb.ts";

/** SessionService client — session lifecycle (create/get/delete/stream/exec). */
export const sessions = createClient(SessionService, controlPlaneTransport);

/** ShellRelayService client — bidi shell relay stream. */
export const shellRelay = createClient(ShellRelayService, controlPlaneTransport);

/** SecretService client — sealed secret management. */
export const secrets = createClient(SecretService, controlPlaneTransport);

/** ImageService client — image prefetch/disable/list. */
export const images = createClient(ImageService, controlPlaneTransport);

/** FleetService client — host view, drain, cordon, GC. */
export const fleet = createClient(FleetService, controlPlaneTransport);
