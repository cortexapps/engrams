/**
 * Per-method policy entries (ADR 0051 Task 18).
 *
 * Gate contract:
 *   - No entry for a method → DENIED (fail-closed principle).
 *   - entry.sessionIdField set → ownership check via resolve.ts + CASL.
 *   - entry.sessionIdField absent → flat CASL check against action/subject.
 *
 * Methods absent from this map:
 *   - StreamEvents, GetArtifact: served by dedicated Hono routes (Task 20),
 *     not the generic passthrough.
 *   - ShellRelayService.Relay: WS route (Task 21), not passthrough.
 *   - TaskService.* : native implementation (Task 19).
 */

import type { DescMethod } from "@bufbuild/protobuf";
import type { Actions, Subjects } from "./ability.ts";

export interface PolicyEntry {
  action: Actions;
  subject: Subjects;
  /**
   * If set, the request message field (camelCase TS name) that holds the
   * control-plane session ID to check ownership for. When present the gate
   * resolves the owner via authz/resolve.ts and constructs a CASL subject.
   */
  sessionIdField?: string;
}

// ---------------------------------------------------------------------------
// Policy map
// ---------------------------------------------------------------------------

/**
 * Maps "ServiceName.MethodName" → PolicyEntry.
 *
 * SessionService: session-scoped methods are member-reachable when owned.
 * ListSessions/CreateSession: admin raw view — UI goes through TaskService.
 * ImageService: read-only subset is member-reachable; mutations are admin.
 * FleetService: every method is admin-only.
 */
export const POLICY: Record<string, PolicyEntry> = {
  // ------------------------------------------------------------------
  // SessionService — session-scoped (member can access own sessions)
  // ------------------------------------------------------------------
  "SessionService.GetSession": {
    action: "read",
    subject: "Session",
    sessionIdField: "sessionId",
  },
  "SessionService.DeleteSession": {
    action: "delete",
    subject: "Session",
    sessionIdField: "sessionId",
  },
  "SessionService.SendPrompt": {
    action: "prompt",
    subject: "Session",
    sessionIdField: "sessionId",
  },
  // Phase 1b (ADR 0052): mutating a still-queued prompt is the same
  // owner-scoped "prompt" capability as sending one.
  "SessionService.EditQueuedPrompt": {
    action: "prompt",
    subject: "Session",
    sessionIdField: "sessionId",
  },
  "SessionService.DequeueQueuedPrompt": {
    action: "prompt",
    subject: "Session",
    sessionIdField: "sessionId",
  },
  // ADR 0054: answering a deferred AskUserQuestion is the same owner-scoped
  // "prompt" capability as sending one — both drive a session you own.
  "SessionService.AnswerQuestion": {
    action: "prompt",
    subject: "Session",
    sessionIdField: "sessionId",
  },
  "SessionService.Interrupt": {
    action: "prompt",
    subject: "Session",
    sessionIdField: "sessionId",
  },
  "SessionService.GetLog": {
    action: "read",
    subject: "Session",
    sessionIdField: "sessionId",
  },
  // ADR 0060: unary catch-up read of the session event log (the reverse-channel
  // pump uses it server-side; through the public passthrough it is the same
  // owner-scoped read as GetLog).
  "SessionService.ListSessionEvents": {
    action: "read",
    subject: "Session",
    sessionIdField: "sessionId",
  },
  "SessionService.GetCowState": {
    action: "read",
    subject: "Session",
    sessionIdField: "sessionId",
  },
  "SessionService.ListCheckpoints": {
    action: "read",
    subject: "Session",
    sessionIdField: "sessionId",
  },
  "SessionService.Exec": {
    action: "shell",
    subject: "Session",
    sessionIdField: "sessionId",
  },
  "SessionService.Resume": {
    action: "read",
    subject: "Session",
    sessionIdField: "sessionId",
  },
  // Admin-only SessionService methods
  "SessionService.Snapshot": { action: "manage", subject: "all" },
  "SessionService.EvictLocal": { action: "manage", subject: "all" },
  "SessionService.EvictIdle": { action: "manage", subject: "all" }, // explicit idle-evict trigger (ADR 0051)
  "SessionService.ListSessions": { action: "manage", subject: "all" }, // raw admin view
  "SessionService.CreateSession": { action: "manage", subject: "all" }, // UI uses CreateTask
  "SessionService.CreateArtifactFromPath": { action: "manage", subject: "all" },

  // ------------------------------------------------------------------
  // ImageService — read subset is member-reachable; mutations are admin
  // ------------------------------------------------------------------
  "ImageService.ListEnabledImages": { action: "read", subject: "EnabledImage" },
  "ImageService.ListEnableJobs": { action: "read", subject: "EnabledImage" },
  "ImageService.GetEnableJob": { action: "read", subject: "EnabledImage" },
  "ImageService.EnableImage": { action: "manage", subject: "all" },
  "ImageService.DisableImage": { action: "manage", subject: "all" },
  "ImageService.RefreshImage": { action: "manage", subject: "all" },
  "ImageService.RetryEnableJob": { action: "manage", subject: "all" },
  "ImageService.ListRegistries": { action: "manage", subject: "all" },
  "ImageService.AddRegistry": { action: "manage", subject: "all" },
  "ImageService.DeleteRegistry": { action: "manage", subject: "all" },

  // ------------------------------------------------------------------
  // HarnessCatalogService (ADR 0063) — read is member-reachable (the
  // harness/model/effort selectors); register/delete are admin (added with
  // the admin Harnesses tab). Only the methods in SURFACE need entries.
  // ------------------------------------------------------------------
  "HarnessCatalogService.ListHarnesses": { action: "read", subject: "Harness" },
  "HarnessCatalogService.GetHarness": { action: "read", subject: "Harness" },

  // ------------------------------------------------------------------
  // FleetService — every method is admin-only
  // ------------------------------------------------------------------
  "FleetService.ListHosts": { action: "manage", subject: "all" },
  "FleetService.GetHost": { action: "manage", subject: "all" },
  "FleetService.GetHostCowState": { action: "manage", subject: "all" },
  "FleetService.DrainHost": { action: "manage", subject: "all" },
  "FleetService.AdminDrainHost": { action: "manage", subject: "all" },
  "FleetService.CordonHost": { action: "manage", subject: "all" },
  "FleetService.UncordonHost": { action: "manage", subject: "all" },
  "FleetService.DeleteHost": { action: "manage", subject: "all" },
  "FleetService.GetStorageSummary": { action: "manage", subject: "all" },
  "FleetService.FlushSession": { action: "manage", subject: "all" },
  "FleetService.EvacuateSession": { action: "manage", subject: "all" },
  "FleetService.ChunkGc": { action: "manage", subject: "all" },
  "FleetService.BundleGc": { action: "manage", subject: "all" },
  "FleetService.SnapshotBlobGc": { action: "manage", subject: "all" },
  "FleetService.GetFleetDemand": { action: "manage", subject: "all" }, // autoscaler demand signal (ADR 0051)
};

/**
 * Derive the policy map key from a service typeName and a DescMethod.
 *
 * Example: typeName="engram.app.v1.SessionService", m.name="getSession"
 *   → "SessionService.GetSession"
 *
 * protobuf-es DescMethod.name is the PascalCase RPC name from the .proto
 * (the same casing as the service descriptor key, e.g. "GetSession").
 */
export function policyKey(serviceTypeName: string, m: DescMethod): string {
  const shortSvc = serviceTypeName.split(".").pop() ?? serviceTypeName;
  return `${shortSvc}.${m.name}`;
}
