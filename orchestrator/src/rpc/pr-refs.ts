/** Orchestrator-native PrRefService (ADR 0100). */

import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";
import type { ConnectRouter } from "@connectrpc/connect";

import { getSessionFromHeaders } from "../auth/session.ts";
import { requireUser } from "./require.ts";
import { getDb } from "../db/client.ts";
import {
  makePrRefStore,
  type PrRefRow,
  type PrRefStore,
} from "../db/pr-refs.ts";
import {
  PrRefService,
  type PrRef,
} from "../gen/engram/app/v1/pr_ref_pb.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{
  user: { id: string };
} | null>;

export interface PrRefDeps {
  getSession?: GetSession;
  prRefs?: PrRefStore;
  db?: ReturnType<typeof getDb>;
}


function toProto(row: PrRefRow): PrRef {
  return {
    id: row.id,
    repo: row.repo,
    prNumber: row.prNumber,
    ...(row.authoringTaskId != null
      ? { authoringTaskId: row.authoringTaskId }
      : {}),
    sessionId: row.sessionId,
    title: row.title,
    url: row.url,
    headBranch: row.headBranch,
    baseBranch: row.baseBranch,
    observedAt: timestampFromDate(row.observedAt),
  } as PrRef;
}

export function registerPrRefs(router: ConnectRouter, deps?: PrRefDeps): void {
  const getSession: GetSession = deps?.getSession ?? getSessionFromHeaders;
  let store = deps?.prRefs;
  const prRefs = (): PrRefStore =>
    (store ??= makePrRefStore(deps?.db ?? getDb()));

  router.service(PrRefService, {
    async listPrRefs(req, ctx) {
      await requireUser(ctx, getSession);

      const taskId = req.taskId?.trim();
      const sessionId = req.sessionId?.trim();
      if ((taskId ? 1 : 0) + (sessionId ? 1 : 0) !== 1) {
        throw new ConnectError(
          "exactly one of task_id or session_id is required",
          Code.InvalidArgument,
        );
      }

      let rows: PrRefRow[];
      if (taskId) {
        rows = await prRefs().listByTaskId(taskId);
      } else if (sessionId) {
        rows = await prRefs().listBySessionId(sessionId);
      } else {
        // The selector count check above makes this unreachable, but keeping
        // the branch explicit preserves the proof for TypeScript too.
        throw new ConnectError(
          "exactly one of task_id or session_id is required",
          Code.InvalidArgument,
        );
      }
      return { prRefs: rows.map(toProto) };
    },
  });
}
