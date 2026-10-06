/** Orchestrator-native RetentionService: the org's retention policy
 * (ADR 0104 amendment, 2026-10-05). Reading is for admins — the Settings
 * page — and so is writing. */

import { Code, ConnectError } from "@connectrpc/connect";
import type { ConnectRouter } from "@connectrpc/connect";

import { getSessionFromHeaders } from "../auth/session.ts";
import { getDb } from "../db/client.ts";
import {
  makeOrgSettingStore,
  readRetentionPolicy,
  retentionPolicySchema,
  RETENTION_KEY,
  RETENTION_RUN_DETAIL_DAYS_MAX,
  RETENTION_RUN_DETAIL_DAYS_MIN,
  type OrgSettingStore,
} from "../db/org-settings.ts";
import { RetentionService } from "../gen/engram/app/v1/retention_pb.ts";
import { requireAdmin, type GetSession } from "./require.ts";

export interface RetentionDeps {
  getSession?: GetSession;
  settings?: OrgSettingStore;
  db?: ReturnType<typeof getDb>;
}

export function registerRetention(router: ConnectRouter, deps?: RetentionDeps): void {
  const getSession: GetSession = deps?.getSession ?? getSessionFromHeaders;
  let store = deps?.settings;
  const settings = (): OrgSettingStore => (store ??= makeOrgSettingStore(deps?.db ?? getDb()));

  router.service(RetentionService, {
    async getRetentionPolicy(_req, ctx) {
      await requireAdmin(ctx, getSession);
      const policy = await readRetentionPolicy(settings());
      return { policy: { runDetailDays: policy.runDetailDays } };
    },

    async setRetentionPolicy(req, ctx) {
      const user = await requireAdmin(ctx, getSession);
      // A missing policy is a bad request, not the default: the schema's
      // defaults exist for an older stored document, never for a write.
      const parsed = req.policy
        ? retentionPolicySchema.safeParse({ runDetailDays: req.policy.runDetailDays })
        : undefined;
      if (parsed === undefined || !parsed.success) {
        throw new ConnectError(
          `run_detail_days must be a whole number of days between ${RETENTION_RUN_DETAIL_DAYS_MIN} and ${RETENTION_RUN_DETAIL_DAYS_MAX}`,
          Code.InvalidArgument,
        );
      }
      await settings().set(RETENTION_KEY, parsed.data, user.id);
      return { policy: { runDetailDays: parsed.data.runDetailDays } };
    },
  });
}
