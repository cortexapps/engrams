import type { McpSessionBindingSnapshot } from "./session-bindings.ts";

/** Non-secret immutable creation association, copied from trusted creation input. */
export interface ManagedCreationIdentity {
  sessionId: string;
  taskId: string;
  inputId: string;
  actorId: string;
  parentSessionId?: string;
  writeEligibility: string;
  inputDigest: string;
}
export interface ManagedBootSourceKey {
  creation: {
    session_id: string;
    task_id: string;
    policy: {
      actorId: string;
      inputId: string;
      parentSessionId: string | null;
      eligibility: string;
    };
    input_digest: string;
  };
  context: {
    target: { session_id: string; sandbox_id: string; host_id: string; binding_epoch: number };
    operator_generation: number;
    operator_identity: string;
    binding_hash: null;
  };
  boot_attempt: string;
  native_digest: string;
}
/** This is progress metadata, never host, capture, placement or effect authority. */
export interface ManagedLifecycle {
  version: 1;
  identity: ManagedCreationIdentity;
  nativeVersion: 1;
  nativeDigest: string;
  phase: "reserving" | "held" | "preparing" | "finalizing" | "started" | "recovering" | "terminal";
  bindingHash: string;
  sourceKey?: ManagedBootSourceKey;
  recovery?: {
    id: string;
    sourceKey: ManagedBootSourceKey;
    expectedBindingHash: string;
    candidateHash: string;
    actorId: string;
    candidate: McpSessionBindingSnapshot;
  };
  pending?: {
    snapshot: McpSessionBindingSnapshot;
    combinedManifestDigest: string;
    preparedReceiptId: string;
  };
}
