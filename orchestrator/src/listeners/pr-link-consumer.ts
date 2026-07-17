import { eq } from "drizzle-orm";
import { z } from "zod";

import type { CuratedEvent } from "../control-plane/session-events.ts";
import { getDb } from "../db/client.ts";
import { makePrRefStore, type PrRefStore } from "../db/pr-refs.ts";
import { taskSession } from "../db/schema.ts";
import { log as rootLog } from "../log.ts";
import type { SessionConsumer } from "./consumer.ts";

const log = rootLog.child({ component: "pr-link-consumer" });

// Only `repo` + `number` are load-bearing (they are the row identity); the
// coordinator marks `fetchable` as `Option` and `data` is whatever the egress
// proxy extracted, so everything decorative degrades to "" rather than
// classifying the event as malformed and silently dropping the task→PR link.
const pullRequestAssetSchema = z.object({
  asset_kind: z.literal("pull_request"),
  data: z.object({
    repo: z.string().min(1),
    number: z.number().int().positive(),
    title: z.string().optional(),
    head_branch: z.string().optional(),
    base_branch: z.string().optional(),
  }),
  fetchable: z
    .discriminatedUnion("kind", [
      z.object({ kind: z.literal("external"), url: z.string() }),
      z.object({ kind: z.literal("artifact") }).passthrough(),
    ])
    .nullish(),
  at: z.string().refine((value) => !Number.isNaN(Date.parse(value))),
});

type PullRequestAsset = z.infer<typeof pullRequestAssetSchema>;

type ParseResult =
  | { kind: "pull_request"; asset: PullRequestAsset }
  | { kind: "ignore" }
  | { kind: "malformed" };

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function parsePullRequestAsset(event: CuratedEvent): ParseResult {
  if (event.kind !== "integration_asset") return { kind: "ignore" };

  let payload: unknown;
  try {
    payload = JSON.parse(event.payloadJson);
  } catch {
    return { kind: "malformed" };
  }
  if (!isRecord(payload)) return { kind: "malformed" };
  if (payload.asset_kind !== "pull_request") return { kind: "ignore" };

  const parsed = pullRequestAssetSchema.safeParse(payload);
  return parsed.success
    ? { kind: "pull_request", asset: parsed.data }
    : { kind: "malformed" };
}

export interface PrLinkConsumerDeps {
  prRefs: PrRefStore;
  findTaskId(sessionId: string): Promise<string | null>;
}

export function makePrLinkConsumer(deps: PrLinkConsumerDeps): SessionConsumer {
  return {
    name: "pr-link",
    interestedIn: (kind) => kind === "integration_asset",
    appliesTo: async () => true,
    async handle(event, ctx) {
      const parsed = parsePullRequestAsset(event);
      if (parsed.kind === "ignore") return;
      if (parsed.kind === "malformed") {
        log.warn(
          { sessionId: ctx.sessionId, eventIdx: event.idx.toString() },
          "skipping malformed pull-request integration asset",
        );
        return;
      }

      const taskId = await deps.findTaskId(ctx.sessionId);
      const { data, fetchable, at } = parsed.asset;
      await deps.prRefs.upsert({
        repo: data.repo,
        prNumber: data.number,
        authoringTaskId: taskId,
        sessionId: ctx.sessionId,
        title: data.title ?? "",
        url: fetchable?.kind === "external" ? fetchable.url : "",
        headBranch: data.head_branch ?? "",
        baseBranch: data.base_branch ?? "",
        observedAt: new Date(at),
      });
    },
  };
}

export function makeProductionPrLinkConsumer(): SessionConsumer {
  const db = getDb();
  return makePrLinkConsumer({
    prRefs: makePrRefStore(db),
    async findTaskId(sessionId) {
      const rows = await db
        .select({ taskId: taskSession.taskId })
        .from(taskSession)
        .where(eq(taskSession.sessionId, sessionId))
        .limit(1);
      return rows[0]?.taskId ?? null;
    },
  });
}
