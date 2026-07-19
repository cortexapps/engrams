/**
 * PR-review enrollment hooks (ADR 0100).
 *
 * A repo must be enrolled here for engrams to review its pull requests — the
 * webhook route drops any event whose repo has no enrollment. Enrollment is
 * admin-only org config on the native Connect `ReviewService`. Each row carries
 * the trigger mode (auto vs. @mention-only), the autofix routing, and an
 * optional profile override (empty = the designated `pr_reviewer` profile).
 */

import { useMutation, useQuery, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import {
  listEnrollments,
  upsertEnrollment,
  deleteEnrollment,
} from "../gen/engram/app/v1/review-ReviewService_connectquery";

/** Every enrolled repo (sorted by repo on the server). */
export function useEnrollments() {
  return useQuery(listEnrollments, {}, { staleTime: 10_000 });
}

function useInvalidateEnrollments() {
  const qc = useQueryClient();
  return () =>
    qc.invalidateQueries({
      queryKey: createConnectQueryKey({
        schema: listEnrollments,
        input: {},
        cardinality: "finite",
      }),
    });
}

/** Enroll a repo or update its settings (upsert keyed on repo). */
export function useUpsertEnrollment() {
  const invalidate = useInvalidateEnrollments();
  return useMutation(upsertEnrollment, { onSuccess: invalidate });
}

/** Un-enroll a repo — future PR events on it are dropped again. */
export function useDeleteEnrollment() {
  const invalidate = useInvalidateEnrollments();
  return useMutation(deleteEnrollment, { onSuccess: invalidate });
}
