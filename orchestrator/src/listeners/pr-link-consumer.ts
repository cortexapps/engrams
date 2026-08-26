import { eq } from "drizzle-orm";
import { z } from "zod";

import type { CuratedEvent } from "../control-plane/session-events.ts";
import { getDb } from "../db/client.ts";
import { makePrRefStore, type PrRefStore } from "../db/pr-refs.ts";
import { makeAutomationEngineStore } from "../db/automations.ts";
import { makeAutomationInstanceStore } from "../db/automation-instances.ts";
import { canonicalHandle } from "../automations/handles.ts";
import { taskSession } from "../db/schema.ts";
import { log as rootLog } from "../log.ts";
import type { SessionConsumer } from "./consumer.ts";

const log = rootLog.child({ component: "pr-link-consumer" });

// Only the row identity (repo + number) is load-bearing, and BOTH may be
// absent from `data` (older connector manifests extracted only number/title;
// GraphQL extraction depends on the client's selection — `gh pr create`'s
// createPullRequest mutation selects only `id`+`url`, so its asset arrives as
// `data: {}`) — each is then derived from the fetchable PR URL. Everything
// decorative degrades to "" rather than classifying the event as malformed
// and silently dropping the task→PR link.
const pullRequestAssetSchema = z.object({
  asset_kind: z.literal("pull_request"),
  data: z.object({
    repo: z.string().min(1).optional(),
    number: z.number().int().positive().optional(),
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

/** Derive the PR identity ("owner/repo" + number) from a forge PR URL like
 * `https://github.com/owner/repo/pull/97`. Returns null when the URL doesn't
 * carry that shape (or isn't a URL at all); `number` is null when the segment
 * after `pull` isn't a positive integer. */
export function prIdentityFromUrl(
  url: string,
): { repo: string; number: number | null } | null {
  if (!url) return null;
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  const segments = parsed.pathname.split("/").filter(Boolean);
  if (segments.length >= 4 && (segments[2] === "pull" || segments[2] === "pulls")) {
    const number = /^[1-9]\d*$/.test(segments[3] ?? "") ? Number(segments[3]) : null;
    return { repo: `${segments[0]}/${segments[1]}`, number };
  }
  return null;
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
  /** ADR 0120: the session's automation binding (engine store). A session an
   * instance-bound run created routes review feedback back through the
   * handle ledger — the consumer writes github:<repo>#<n> on PR open. */
  findAutomationBinding?(sessionId: string): Promise<{
    automationId: string;
    instanceId: string;
  } | null>;
  recordInstanceHandle?(input: {
    automationId: string;
    handle: string;
    instanceId: string;
    writtenBy: string;
  }): Promise<{ kind: "recorded" } | { kind: "already_ours" } | { kind: "conflict"; instanceId: string }>;
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

      const { data, fetchable, at } = parsed.asset;
      const url = fetchable?.kind === "external" ? fetchable.url : "";
      const fromUrl = prIdentityFromUrl(url);
      const repo = data.repo ?? fromUrl?.repo;
      const prNumber = data.number ?? fromUrl?.number;
      if (!repo || !prNumber) {
        log.warn(
          { sessionId: ctx.sessionId, eventIdx: event.idx.toString() },
          "pull-request asset carries no repo/number and neither is derivable from its URL; skipping",
        );
        return;
      }

      const taskId = await deps.findTaskId(ctx.sessionId);
      await deps.prRefs.upsert({
        repo,
        prNumber,
        authoringTaskId: taskId,
        sessionId: ctx.sessionId,
        title: data.title ?? "",
        url,
        headBranch: data.head_branch ?? "",
        baseBranch: data.base_branch ?? "",
        observedAt: new Date(at),
      });

      // ADR 0120: bind the PR to the session's workstream so review events
      // route back (handle admission). Best-effort AND loud: a conflict
      // means another workstream already owns this PR — log, never rebind,
      // and never fail the consumer (the pr_ref upsert above stands).
      if (deps.findAutomationBinding && deps.recordInstanceHandle) {
        const binding = await deps.findAutomationBinding(ctx.sessionId);
        if (binding !== null && binding.instanceId !== "") {
          const handle = canonicalHandle("github", `github:${repo}#${prNumber}`);
          const result = await deps.recordInstanceHandle({
            automationId: binding.automationId,
            handle,
            instanceId: binding.instanceId,
            writtenBy: `consumer:pr-link:${ctx.sessionId}`,
          });
          if (result.kind === "conflict") {
            log.warn(
              { sessionId: ctx.sessionId, handle, holder: result.instanceId },
              "pr handle already routes to another workstream; NOT rebinding",
            );
          }
        }
      }
    },
  };
}

export function makeProductionPrLinkConsumer(): SessionConsumer {
  const db = getDb();
  const engine = makeAutomationEngineStore();
  const instances = makeAutomationInstanceStore();
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
    async findAutomationBinding(sessionId) {
      const binding = await engine.getSessionBinding(sessionId);
      return binding
        ? { automationId: binding.automationId, instanceId: binding.instanceId }
        : null;
    },
    recordInstanceHandle: (input) => instances.recordInstanceHandle(input),
  });
}
