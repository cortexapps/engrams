/** The PR-review built-in, driven end to end through the REAL interpreter
 * (immediate steps + scripted receiver), with the control plane, session
 * ops, and action runtime faked. This is the replacement for the legacy
 * pr-review workflow tests (ADR 0119 phase 4.7 deletes those); every phase
 * the legacy graph proved is proved here against the definition as data.
 */

import { afterEach, describe, expect, test } from "bun:test";

import type { BeginReviewPassResult } from "../../../db/reviews.ts";
import type { ReviewPostPayload } from "../../../reviews/control-plane.ts";
import { makeCodeBlockRuntime } from "../../code/runtime.ts";
import { registerEngineBlocks } from "../../engine/blocks/index.ts";
import {
  setReviewBlockDeps,
  type ReviewBlockControlPlane,
} from "../../engine/blocks/system/review.ts";
import type { RunSnapshot } from "../../engine/context.ts";
import type { EngineDeps, EngineSessionOps, EngineStepRecord } from "../../engine/deps.ts";
import type { AutomationInbox } from "../../engine/inbox.ts";
import { interpretAutomation } from "../../engine/interpreter.ts";
import { PR_REVIEW_DEFINITION } from "../pr-review.ts";

registerEngineBlocks();
afterEach(() => setReviewBlockDeps(null));

const HEAD = "a".repeat(40);
const BASE = "b".repeat(40);
const RUN = { runId: "autorun:auto-pr:github:d1", automationId: "auto-pr" };

const INPUTS = {
  repos: { "acme/repo": { mode: "auto", autofix: false }, "acme/other": { mode: "on_request", autofix: false } },
  profile: "pr_reviewer",
  mention: "@engrams",
  categories: ["functional-correctness", "security-privacy"],
  instructions: "Be terse.",
};

function prPayload(overrides: { draft?: boolean; repo?: string } = {}): Record<string, unknown> {
  const repo = overrides.repo ?? "acme/repo";
  return {
    action: "opened",
    repository: { full_name: repo, name: repo.split("/")[1] },
    pull_request: {
      id: 4242,
      number: 17,
      draft: overrides.draft ?? false,
      html_url: `https://github.com/${repo}/pull/17`,
      title: "Fix the thing",
      user: { login: "dev" },
      state: "open",
      updated_at: "2026-08-21T10:00:00Z",
      head: { sha: HEAD, ref: "fix" },
      base: { sha: BASE, ref: "main" },
      additions: 3,
      deletions: 1,
      changed_files: 2,
    },
  };
}

function commentPayload(body: string, assoc = "MEMBER"): Record<string, unknown> {
  return {
    action: "created",
    repository: { full_name: "acme/other", name: "other" },
    issue: { number: 5, pull_request: { url: "x" }, html_url: "https://github.com/acme/other/pull/5" },
    comment: { body, author_association: assoc },
    sender: { type: "User" },
  };
}

const PAYLOAD: ReviewPostPayload = {
  review_id: "review-1",
  repo: "acme/repo",
  pr_number: 17,
  commit_id: HEAD,
  summary_md: "Summary\n\n<!-- engrams-review:review-1 -->",
  comments: [{ finding_id: "f1", path: "src/a.ts", line: 3, side: "RIGHT", body: "nit" }],
  to_post_count: 1,
  ui_only_count: 0,
};

interface Harness {
  deps: EngineDeps;
  names: string[];
  records: Array<{ path: string; attempt: number; record: EngineStepRecord }>;
  cpCalls: string[];
  sessions: Array<{ id: string; role: string; capabilityOverride?: readonly string[]; appendSystemPrompt?: string; keep: boolean }>;
  prompts: Array<{ sessionId: string; text: string }>;
  execs: Array<{ sessionId: string; command: string }>;
  ended: string[];
  actions: Array<{ actionId: string; params: Record<string, unknown> }>;
  stamped: Array<{ reviewId: string; runId: string }>;
  finalized: Array<{ status: string; error?: string }>;
}

function harness(options: {
  eventKey: string;
  payload: Record<string, unknown>;
  recv?: AutomationInbox[];
  deduplicate?: boolean;
}): Harness {
  const names: string[] = [];
  const records: Harness["records"] = [];
  const cpCalls: string[] = [];
  const sessions: Harness["sessions"] = [];
  const prompts: Harness["prompts"] = [];
  const execs: Harness["execs"] = [];
  const ended: string[] = [];
  const actions: Harness["actions"] = [];
  const stamped: Harness["stamped"] = [];
  const finalized: Harness["finalized"] = [];
  const recvQueue = [...(options.recv ?? [])];
  const runSessions: Array<{ sessionId: string; keep: boolean }> = [];
  let clock = 1_000_000;

  const cp: ReviewBlockControlPlane = {
    async resolvePrHeads() {
      cpCalls.push("resolvePrHeads");
      return {
        headSha: HEAD,
        baseSha: BASE,
        pr: {
          providerId: "4242", title: "Fix the thing", author: "dev", state: "open",
          url: "https://github.com/acme/other/pull/5", providerUpdatedAt: new Date("2026-08-21T10:00:00Z"),
          headBranch: "fix", baseBranch: "main", additions: 3, deletions: 1, changedFiles: 2,
        },
      };
    },
    async resolveReviewTarget() {
      cpCalls.push("resolveReviewTarget");
      return { targetId: "target-1" };
    },
    async createReviewPass(input): Promise<BeginReviewPassResult> {
      cpCalls.push(`createReviewPass:${input.trigger}`);
      if (options.deduplicate) return { kind: "deduplicated", reviewId: "review-0", taskId: "task-0" };
      return { kind: "created", reviewId: "review-1", taskId: "task-1" };
    },
    async bootstrapFinderSession(_sid, input) {
      cpCalls.push(`bootstrapFinder:skipClone=${String(input.skipClone)}:cats=${(input.enabledCategories ?? []).join("+")}`);
    },
    async bootstrapVerifierSession(_sid, input) {
      cpCalls.push(`bootstrapVerifier:skipClone=${String(input.skipClone)}`);
    },
    async composeFinderPrompt() {
      cpCalls.push("composeFinderPrompt");
      return { prompt: "FINDER PROMPT", mergeBase: BASE };
    },
    composeVerifierPrompt() {
      cpCalls.push("composeVerifierPrompt");
      return { prompt: "VERIFIER PROMPT" };
    },
    async markPhasePrompted(_id, role) {
      cpCalls.push(`markPhasePrompted:${role}`);
    },
    async decideReviewResults() {
      cpCalls.push("decideReviewResults");
      return PAYLOAD;
    },
    async cleanupSupersededReview() {
      cpCalls.push("cleanupSupersededReview");
    },
  };
  setReviewBlockDeps({
    controlPlane: () => cp,
    reviews: () => ({
      async setAutomationRunId(reviewId, runId) {
        stamped.push({ reviewId, runId });
      },
    }),
  });

  const snapshot: RunSnapshot = {
    definition: { ...PR_REVIEW_DEFINITION, trigger: { ...PR_REVIEW_DEFINITION.trigger, ...(PR_REVIEW_DEFINITION.trigger.kind === "integration" ? { connectionId: "conn-github" } : {}) } },
    inputs: INPUTS,
    automationId: RUN.automationId,
    automationName: "PR review",
    version: 1,
    trigger: {
      kind: "integration",
      receivedAt: "2026-08-21T10:00:01Z",
      eventKey: options.eventKey,
      deliveryKey: "github:d1",
      payload: options.payload,
    },
    aliases: [],
    startedAtMs: clock,
  };

  const sessionOps: EngineSessionOps = {
    async createSession(input) {
      const id = `s-${input.role}`;
      sessions.push({
        id,
        role: input.role,
        keep: input.keep,
        ...(input.capabilityOverride ? { capabilityOverride: input.capabilityOverride } : {}),
        ...(input.appendSystemPrompt ? { appendSystemPrompt: input.appendSystemPrompt } : {}),
      });
      runSessions.push({ sessionId: id, keep: input.keep });
      return { sessionId: id, taskId: `t-${input.role}` };
    },
    async sendPrompt(sessionId, _promptId, text) {
      prompts.push({ sessionId, text });
    },
    async endSession(sessionId) {
      ended.push(sessionId);
    },
    async exec(sessionId, command) {
      execs.push({ sessionId, command });
      return { exitStatus: 0, stdout: "", stderr: "" };
    },
    async writeFiles(_s, files) {
      return files.map((f) => ({ path: f.path, ok: true }));
    },
  };

  const deps: EngineDeps = {
    step: async (fn, name) => {
      names.push(name);
      return fn();
    },
    recv: async () => recvQueue.shift() ?? null,
    store: {
      async loadSnapshot() { return snapshot; },
      async markRunning() {},
      async recordStep(_r, path, attempt, record) { records.push({ path, attempt, record }); },
      async finalizeRun(_r, status, error) { finalized.push({ status, ...(error !== undefined ? { error } : {}) }); },
      async listRunSessions() { return runSessions; },
      async releaseConcurrency() { return null; },
    },
    sessions: sessionOps,
    clock: { nowMs: () => (clock += 1000) },
    code: makeCodeBlockRuntime(),
    integrationActions: {
      async execute(input) {
        actions.push({ actionId: input.actionId, params: input.params });
        if (input.actionId === "create_issue_comment") return { commentId: 777, status: 201 };
        if (input.actionId === "post_pr_review") return { reviewId: 9001, status: 200 };
        return { status: 200 };
      },
    },
  };

  return { deps, names, records, cpCalls, sessions, prompts, execs, ended, actions, stamped, finalized };
}

const finderDone = (count: number): AutomationInbox => ({
  kind: "signal",
  name: "finder_done",
  sessionId: "s-finder",
  payload: { candidate_count: count },
});
const verifierDone: AutomationInbox = { kind: "signal", name: "verifier_done", sessionId: "s-verifier", payload: {} };

describe("PR-review built-in on the interpreter", () => {
  test("(a) zero candidates: finder only, gate, post, status — no verifier session", async () => {
    const h = harness({ eventKey: "pull_request.opened", payload: prPayload(), recv: [finderDone(0)] });

    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    expect(h.cpCalls).toEqual([
      "resolveReviewTarget",
      "createReviewPass:opened",
      "bootstrapFinder:skipClone=true:cats=functional-correctness+security-privacy",
      "composeFinderPrompt",
      "markPhasePrompted:finder",
      "decideReviewResults",
    ]);
    // Heads came from the payload: no GitHub round-trip.
    expect(h.cpCalls).not.toContain("resolvePrHeads");
    expect(h.stamped).toEqual([{ reviewId: "review-1", runId: RUN.runId }]);
    expect(h.sessions.map((s) => s.role)).toEqual(["finder"]);
    expect(h.sessions[0]).toMatchObject({
      keep: false,
      capabilityOverride: ["engram:pr_review", "github:contents:read@acme/repo"],
    });
    expect(h.sessions[0]!.appendSystemPrompt).toContain("finder");
    // Visible clone at the PR head, then the staged prompt delivered.
    expect(h.execs[0]!.command).toContain(`git clone https://github.com/acme/repo.git /workspace/repo`);
    expect(h.execs[0]!.command).toContain(`checkout ${HEAD}`);
    expect(h.prompts).toEqual([{ sessionId: "s-finder", text: "FINDER PROMPT" }]);
    // Ack → post (structured comments passed by $ref) → status update.
    expect(h.actions.map((a) => a.actionId)).toEqual([
      "create_issue_comment",
      "post_pr_review",
      "update_issue_comment",
    ]);
    expect(h.actions[1]!.params).toMatchObject({
      repo: "acme/repo",
      prNumber: 17,
      commitId: HEAD,
      comments: PAYLOAD.comments,
    });
    expect(h.actions[2]!.params).toMatchObject({ commentId: 777 });
    expect(String(h.actions[2]!.params["body"])).toContain("1 finding");
    // Explicit end_session ran; finalize found nothing left to end.
    expect(h.ended).toEqual(["s-finder"]);
    expect(h.finalized).toEqual([{ status: "completed" }]);
  });

  test("(b) candidates: the verifier pair runs before the gate", async () => {
    const h = harness({ eventKey: "pull_request.opened", payload: prPayload(), recv: [finderDone(3), verifierDone] });

    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    expect(h.sessions.map((s) => s.role)).toEqual(["finder", "verifier"]);
    expect(h.cpCalls).toContain("bootstrapVerifier:skipClone=true");
    expect(h.cpCalls).toContain("markPhasePrompted:verifier");
    expect(h.prompts.map((p) => p.text)).toEqual(["FINDER PROMPT", "VERIFIER PROMPT"]);
    expect(h.execs).toHaveLength(2);
    // Verifier ends inside the branch, finder after it, gate after both.
    expect(h.ended).toEqual(["s-verifier", "s-finder"]);
    const gateIdx = h.cpCalls.indexOf("decideReviewResults");
    expect(gateIdx).toBeGreaterThan(h.cpCalls.indexOf("markPhasePrompted:verifier"));
    expect(h.names).toContain("step:has_candidates.__cond__:0");
    expect(h.names).toContain("step:has_candidates.verifier:0");
  });

  test("(c) a same-head redelivery is deduplicated into a filtered run before any session", async () => {
    const h = harness({ eventKey: "pull_request.synchronize", payload: prPayload(), deduplicate: true });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("filtered");
    expect(h.sessions).toEqual([]);
    expect(h.actions).toEqual([]);
  });

  test("(d) the admission predicate rejects a draft PR, an unmapped repo, and a non-member command", async () => {
    const draft = harness({ eventKey: "pull_request.opened", payload: prPayload({ draft: true }) });
    expect((await interpretAutomation(RUN, draft.deps)).status).toBe("filtered");
    expect(draft.cpCalls).toEqual([]);

    const unmapped = harness({ eventKey: "pull_request.opened", payload: prPayload({ repo: "stranger/repo" }) });
    expect((await interpretAutomation(RUN, unmapped.deps)).status).toBe("filtered");

    const outsider = harness({ eventKey: "issue_comment.created", payload: commentPayload("@engrams review", "NONE") });
    expect((await interpretAutomation(RUN, outsider.deps)).status).toBe("filtered");
  });

  test("(d') a member's review command on an on_request repo resolves heads from GitHub and runs as a command", async () => {
    const h = harness({ eventKey: "issue_comment.created", payload: commentPayload("@engrams review"), recv: [finderDone(0)] });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.error).toBeUndefined();
    expect(result.status).toBe("completed");
    expect(h.cpCalls.slice(0, 3)).toEqual(["resolvePrHeads", "resolveReviewTarget", "createReviewPass:command"]);
    expect(h.sessions[0]!.capabilityOverride).toEqual(["engram:pr_review", "github:contents:read@acme/other"]);
  });

  test("(e) a supersede mid-finder ends the run as superseded and finalize ends the kept=false workers", async () => {
    const h = harness({
      eventKey: "pull_request.opened",
      payload: prPayload(),
      recv: [{ kind: "supersede", byRunId: "autorun:auto-pr:github:d2" }],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("superseded");
    expect(h.prompts).toHaveLength(1);
    expect(h.cpCalls).not.toContain("decideReviewResults");
    expect(h.actions.map((a) => a.actionId)).toEqual(["create_issue_comment"]);
    // No explicit end_session ran (the graph was cut short); finalize ended
    // the finder because the built-in creates workers with keep=false.
    expect(h.ended).toEqual(["s-finder"]);
    expect(h.finalized[0]!.status).toBe("superseded");
  });
});
