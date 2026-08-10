/** Native, database-only SpecService list surface. */

import { Code, ConnectError, type ConnectRouter } from "@connectrpc/connect";

import { abilityFor } from "../authz/ability.ts";
import { config } from "../config.ts";
import { makeSpecListStore, type SpecLifecycle, type SpecListStore } from "../db/specs.ts";
import { SpecService } from "../gen/engram/app/v1/spec_pb.ts";
import { requireUser, type GetSession } from "./require.ts";

const MAX_PAGE_SIZE = 200;

export interface SpecRpcDeps {
  getSession?: GetSession;
  store?: SpecListStore;
  orgId?: string;
}

function lifecycleOf(value: string): SpecLifecycle | undefined {
  if (value === "" || value === "all") return undefined;
  if (value === "draft" || value === "published") return value;
  throw new ConnectError("invalid lifecycle", Code.InvalidArgument);
}

export function registerSpecs(router: ConnectRouter, deps: SpecRpcDeps = {}): void {
  const store = deps.store ?? makeSpecListStore();
  const orgId = deps.orgId ?? config.deploymentId;

  router.service(SpecService, {
    async listSpecs(req, ctx) {
      const actor = await requireUser(ctx, deps.getSession);
      const member = await store.isMember(actor.id);
      if (!member || !abilityFor(actor).can("read", "Spec")) {
        throw new ConnectError("not found", Code.NotFound);
      }

      const lifecycle = lifecycleOf(req.lifecycle);
      const { rows, totalCount } = await store.list({
        orgId,
        ...(lifecycle ? { lifecycle } : {}),
        page: Math.max(req.page, 1),
        pageSize: req.pageSize > 0 ? Math.min(req.pageSize, MAX_PAGE_SIZE) : MAX_PAGE_SIZE,
      });

      return {
        specs: rows.map((row) => ({
          id: row.id,
          title: row.title,
          templateName: row.templateName,
          ...(row.repo ? { repo: row.repo } : {}),
          lifecycle: row.lifecycle,
          participants: row.participants,
          activeParticipantCount: row.activeParticipantCount,
          openQuestionCount: row.openQuestionCount,
          ticketSyncState: row.ticketSyncState,
          updatedAt: row.updatedAt.toISOString(),
        })),
        totalCount,
      };
    },
  });
}
