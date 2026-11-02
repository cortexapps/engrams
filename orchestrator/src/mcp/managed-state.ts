import { z } from "zod";
import { canonicalJson } from "./hash.ts";
import type { ManagedCreationIdentity, ManagedLifecycle } from "./managed-lifecycle.ts";
import { bytesDigest } from "./managed-create-input.ts";

const id = z
  .string()
  .min(1)
  .max(128)
  .regex(/^[a-zA-Z0-9-]+$/);
const actor = z
  .string()
  .min(1)
  .max(256)
  .regex(/^[^\u0000-\u001f]+$/);
const hash = z.string().regex(/^sha256:[0-9a-f]{64}$/);
const integer = z.number().int().safe().nonnegative();
export const managedIdentitySchema = z
  .object({
    session_id: id,
    task_id: actor,
    policy: z
      .object({
        actorId: actor,
        inputId: actor,
        parentSessionId: id.nullable(),
        eligibility: z.enum(["read_only", "write_eligible"]),
      })
      .strict(),
    input_digest: hash,
  })
  .strict();
export const managedSourceSchema = z
  .object({
    creation: managedIdentitySchema,
    context: z
      .object({
        target: z
          .object({
            session_id: id,
            sandbox_id: id,
            host_id: id,
            binding_epoch: integer.positive(),
          })
          .strict(),
        operator_generation: integer.positive(),
        operator_identity: hash,
        binding_hash: z.null(),
      })
      .strict(),
    boot_attempt: id,
    native_digest: hash,
  })
  .strict();
const deny = z
  .object({
    version: z.literal(1),
    revision: integer,
    hosts: z.array(z.string()).max(4096),
    digest: hash,
  })
  .strict();
const request = z.object({ key: managedSourceSchema, bootstrap_policy: deny });
const prepared = z.object({
  request,
  host_incarnation: id,
  network_identity: hash,
  guest_ip: z.string(),
  receipt_id: hash,
});
const boot = z.object({
  request,
  revision: integer.positive(),
  phase: z.enum([
    "claimed",
    "prepared",
    "finalizing",
    "finalized",
    "start_issued",
    "started",
    "interrupted",
  ]),
  prepared: prepared.nullable(),
  sealed_agent: z.null(),
  retired: z.object({ prepared, retained_policy: deny, receipt_id: hash }).nullable(),
  finalization: z
    .object({
      prepared,
      native_version: z.literal(1),
      combined_manifest: z.array(z.number().int().min(0).max(255)).max(1024 * 1024),
      combined_manifest_digest: hash,
    })
    .nullable(),
});
const state = z
  .object({
    query: z
      .object({
        identity: managedIdentitySchema,
        native_version: z.literal(1),
        native_digest: hash,
      })
      .strict(),
    state: z.enum(["absent", "held", "preparing", "boot", "recovering", "terminal"]),
    boot: boot.nullable(),
    operation: z
      .object({
        intent_id: id,
        revision: integer.positive(),
        recovery_id: id.nullable(),
        source_key: managedSourceSchema.nullable(),
        capture_id: id.nullable(),
        completed: z.boolean(),
      })
      .strict()
      .nullable(),
    terminal_reason: z.string().max(256).nullable(),
  })
  .strict();
export type ManagedCreationState = z.infer<typeof state>;
export function rustCreationIdentity(value: ManagedCreationIdentity) {
  return managedIdentitySchema.parse({
    session_id: value.sessionId,
    task_id: value.taskId,
    policy: {
      actorId: value.actorId,
      inputId: value.inputId,
      parentSessionId: value.parentSessionId ?? null,
      eligibility: value.writeEligibility,
    },
    input_digest: value.inputDigest,
  });
}
export function managedQuery(value: ManagedLifecycle) {
  return {
    identity: rustCreationIdentity(value.identity),
    native_version: value.nativeVersion,
    native_digest: value.nativeDigest,
  };
}
/** Decode only the sanitized producer view. Missing or mismatched state is never an ACK. */
export function decodeManagedState(
  response: { protocol: number; stateJson: Uint8Array },
  expected: ManagedLifecycle,
) {
  if (
    response.protocol !== 1 ||
    !response.stateJson.length ||
    response.stateJson.length > 4 * 1024 * 1024
  )
    throw Error("MCP creation state unavailable");
  const value = state.parse(
    JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(response.stateJson)),
  );
  if (canonicalJson(value.query) !== canonicalJson(managedQuery(expected)))
    throw Error("MCP creation identity changed");
  if (value.state === "absent" && (value.boot || value.operation || value.terminal_reason))
    throw Error("MCP absent state changed");
  const key = value.boot?.request.key;
  if (
    key &&
    (canonicalJson(key.creation) !== canonicalJson(value.query.identity) ||
      key.native_digest !== expected.nativeDigest ||
      key.context.target.session_id !== expected.identity.sessionId)
  )
    throw Error("MCP boot identity changed");
  for (const receipt of [
    value.boot?.prepared,
    value.boot?.finalization?.prepared,
    value.boot?.retired?.prepared,
  ]) {
    if (receipt && canonicalJson(receipt.request.key) !== canonicalJson(key))
      throw Error("MCP prepared identity changed");
  }
  const finalization = value.boot?.finalization;
  if (
    finalization &&
    bytesDigest(new Uint8Array(finalization.combined_manifest)) !==
      finalization.combined_manifest_digest
  )
    throw Error("MCP final manifest changed");
  const operation = value.operation;
  if (
    operation &&
    (Boolean(operation.recovery_id) !== Boolean(operation.source_key) ||
      Boolean(operation.source_key) !== Boolean(operation.capture_id))
  )
    throw Error("MCP recovery association unavailable");
  if (
    operation?.source_key &&
    (canonicalJson(operation.source_key.creation) !== canonicalJson(value.query.identity) ||
      operation.source_key.native_digest !== expected.nativeDigest)
  )
    throw Error("MCP recovery source changed");
  return value;
}
