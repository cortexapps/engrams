/**
 * The passthrough surface specification (ADR 0039 Task 18).
 *
 * SURFACE lists every service and method that the orchestrator forwards to
 * the control plane. The following are intentionally absent:
 *
 *   - ShellRelayService      WS route (Task 21)
 *   - SecretService          keys come from the session, not the user surface
 *   - TaskService            native implementation (Task 19)
 *   - StreamEvents           dedicated Hono SSE route (Task 20)
 *   - GetArtifact            dedicated Hono byte-streaming route (Task 20)
 *
 * SessionService.StreamEvents and SessionService.GetArtifact are excluded
 * from the SessionService forwarding list — their filtering is applied here
 * so the conformance test does not try to exercise them through the passthrough.
 */

import { SessionService } from "../gen/engram/app/v1/session_pb.ts";
import { FleetService } from "../gen/engram/app/v1/fleet_pb.ts";
import { ImageService } from "../gen/engram/app/v1/image_pb.ts";
import type { PassthroughSpec } from "./passthrough.ts";

/**
 * Methods excluded from the SessionService passthrough — each has its own
 * dedicated route.
 */
const SESSION_EXCLUDED: ReadonlySet<string> = new Set([
  "StreamEvents", // Hono SSE route (Task 20)
  "GetArtifact",  // Hono byte-stream route (Task 20)
]);

/**
 * The ordered list of passthrough specs fed to registerPassthrough.
 *
 * - SessionService: all methods except StreamEvents/GetArtifact.
 * - FleetService: all methods (all admin-only).
 * - ImageService: all methods.
 */
export const SURFACE: PassthroughSpec[] = [
  {
    service: SessionService,
    // Enumerate method names explicitly so the list is stable and visible.
    methods: SessionService.methods
      .filter((m) => !SESSION_EXCLUDED.has(m.name))
      .map((m) => m.name),
  },
  {
    service: FleetService,
    // No filter — all 13 methods forwarded.
  },
  {
    service: ImageService,
    // No filter — all 10 methods forwarded.
  },
];
