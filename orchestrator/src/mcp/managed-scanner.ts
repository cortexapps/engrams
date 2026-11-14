import { Code, ConnectError } from "@connectrpc/connect";
import type { makeManagedLifecycleStore } from "./managed-lifecycle-store.ts";
import type { makeManagedSessionRuntime } from "./managed-runtime.ts";
import { log } from "../log.ts";

// Only fixed local failures may select a reason. Never log the caught error.
const reconciliationReasons = new Map([
  ["MCP prepared receipt unavailable", "prepared_receipt_unavailable"],
  ["MCP boot publication changed", "boot_publication_changed"],
  ["MCP frozen native manifest changed", "frozen_native_manifest_changed"],
  ["MCP final binding reservation missing", "final_binding_reservation_missing"],
  ["MCP retained policy presence unavailable", "retained_policy_presence_unavailable"],
  ["MCP pending boot changed", "pending_boot_changed"],
  ["MCP final manifest changed", "final_manifest_changed"],
  ["MCP managed apply unavailable", "managed_apply_unavailable"],
  ["MCP managed apply changed", "managed_apply_changed"],
  ["MCP finalization reservation changed", "finalization_reservation_changed"],
  ["MCP recovery operation changed", "recovery_operation_changed"],
  ["MCP finalized manifest changed", "finalized_manifest_changed"],
  ["MCP finalization response unavailable", "finalization_response_unavailable"],
  ["MCP managed integer unavailable", "managed_integer_unavailable"],
  ["MCP managed policy unavailable", "managed_policy_unavailable"],
  ["MCP managed target unavailable", "managed_target_unavailable"],
  ["MCP managed receipt unavailable", "managed_receipt_unavailable"],
  ["MCP network proof unavailable", "network_proof_unavailable"],
  ["MCP managed receipt digest changed", "managed_receipt_digest_changed"],
  ["MCP managed revision exhausted", "managed_revision_exhausted"],
  ["MCP applied target changed", "applied_target_changed"],
  ["MCP managed binding changed", "managed_binding_changed"],
  ["MCP final managed acknowledgement unavailable", "final_managed_acknowledgement_unavailable"],
]);
const rpcReasons = new Map([
  [Code.Canceled, "rpc_canceled"],
  [Code.Unknown, "rpc_unknown"],
  [Code.InvalidArgument, "rpc_invalid_argument"],
  [Code.DeadlineExceeded, "rpc_deadline_exceeded"],
  [Code.NotFound, "rpc_not_found"],
  [Code.AlreadyExists, "rpc_already_exists"],
  [Code.PermissionDenied, "rpc_permission_denied"],
  [Code.ResourceExhausted, "rpc_resource_exhausted"],
  [Code.FailedPrecondition, "rpc_failed_precondition"],
  [Code.Aborted, "rpc_aborted"],
  [Code.OutOfRange, "rpc_out_of_range"],
  [Code.Unimplemented, "rpc_unimplemented"],
  [Code.Internal, "rpc_internal"],
  [Code.Unavailable, "rpc_unavailable"],
  [Code.DataLoss, "rpc_data_loss"],
  [Code.Unauthenticated, "rpc_unauthenticated"],
]);

/** Durable keyset scan. A failed row cannot starve later reservations. */
export function makeManagedSessionScanner(deps: {
  store: Pick<ReturnType<typeof makeManagedLifecycleStore>, "pending">;
  runtime: Pick<ReturnType<typeof makeManagedSessionRuntime>, "runOnce">;
}) {
  let after: string | undefined;
  let stopped = true;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let running: Promise<void> | undefined;
  async function runOnce(stopBetweenRows = false) {
    const rows = await deps.store.pending(after);
    for (const row of rows) {
      if (stopBetweenRows && stopped) break;
      try {
        await deps.runtime.runOnce(row.sessionId);
      } catch (error) {
        const reason =
          error instanceof ConnectError
            ? (rpcReasons.get(error.code) ?? "unclassified")
            : error instanceof Error
              ? (reconciliationReasons.get(error.message) ?? "unclassified")
              : "unclassified";
        log.warn({ reason }, "MCP managed session reconciliation unavailable");
      }
      after = row.sessionId;
    }
    if (rows.length === 0) after = undefined;
  }
  async function tick() {
    try {
      await runOnce(true);
    } catch {
      log.warn("MCP managed session scan unavailable");
    }
    if (!stopped)
      timer = setTimeout(() => {
        running = tick();
      }, 1000);
  }
  return {
    runOnce: () => runOnce(),
    async start() {
      if (!stopped) return;
      stopped = false;
      running = tick();
      await running;
    },
    async stop() {
      stopped = true;
      clearTimeout(timer);
      await running;
    },
  };
}
