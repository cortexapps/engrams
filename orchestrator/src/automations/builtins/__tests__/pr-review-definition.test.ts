import { describe, expect, test } from "bun:test";

import { registerEngineBlocks } from "../../engine/blocks/index.ts";
import { evaluateAdmissionPrelude } from "../../engine/admission.ts";
import { validateDefinition, applyBlockOverrides } from "../../engine/definition.ts";
import { admissionPrelude, previewDefinition } from "../../engine/preview.ts";
import { makeCodeBlockRuntime } from "../../code/runtime.ts";
import { renderAutomationTemplateInScope } from "../../template.ts";
import { requireForge } from "../../../forges/registry.ts";
import { reviewFacts } from "../../../forges/review-facts.ts";
import { reviewBuiltin, reviewDefinition } from "../review.ts";
import { GITHUB_REVIEW, defaultMentionHandle } from "../../../providers/github/review-variant.ts";

const PR_REVIEW_BUILTIN = reviewBuiltin(GITHUB_REVIEW);
const PR_REVIEW_DEFINITION = reviewDefinition(GITHUB_REVIEW);

registerEngineBlocks();

const inputs = {
  repos: { "Acme/Repo": { mode: "auto", autofix: false }, "acme/manual": { mode: "on_request", autofix: false } },
  profile: "11111111-2222-4333-8444-555555555555",
  mention: "@engrams",
  categories: ["functional-correctness"],
  instructions: "Prefer small diffs.",
};

function prEvent(action: string, overrides: Record<string, unknown> = {}) {
  return {
    raw: {
      action,
      repository: { full_name: "acme/repo", name: "repo" },
      pull_request: {
        id: 42,
        number: 7,
        draft: false,
        html_url: "https://github.com/acme/repo/pull/7",
        title: "Fix",
        user: { login: "dev" },
        state: "open",
        updated_at: "2026-08-21T00:00:00Z",
        head: { sha: "a".repeat(40), ref: "fix" },
        base: { sha: "b".repeat(40), ref: "main" },
        additions: 1,
        deletions: 0,
        changed_files: 1,
        ...overrides,
      },
    },
  };
}

function commentEvent(body: string, assoc = "MEMBER", senderType = "User", repo = "acme/manual") {
  return {
    raw: {
      action: "created",
      repository: { full_name: repo, name: repo.split("/")[1] },
      issue: { number: 9, pull_request: { url: "x" }, html_url: `https://github.com/${repo}/pull/9` },
      comment: { body, author_association: assoc, user: { login: "dev" } },
      sender: { type: senderType },
    },
  };
}

describe("PR_REVIEW_BUILTIN definition", () => {
  test("validates as a builtin AND as a user definition: every block is a palette block", () => {
    // The built-in is an example an ordinary user could have built. A
    // Duplicate copies it into a user-kind row, so a user-kind validation
    // failure here means Duplicate produces an unsaveable copy.
    expect(() => validateDefinition(PR_REVIEW_DEFINITION)).not.toThrow();
    expect(() => validateDefinition(PR_REVIEW_DEFINITION)).not.toThrow();
  });

  test("is a workstream per pull request (ADR 0120): one key for every way in, closed on PR close", async () => {
    const instance = PR_REVIEW_DEFINITION.settings.instance!;
    const scope = (raw: Record<string, unknown>, event: string) => ({
      trigger: { kind: "integration", event },
      event: { raw },
      inputs,
    });
    // A PR event names the PR; a comment names the issue; the CI dispatch
    // and a retry carry pull_request.number — all render the same key.
    expect(await renderAutomationTemplateInScope(instance.keyTemplate, scope(prEvent("opened").raw, "pull_request.opened"))).toBe("acme/repo#7");
    expect(await renderAutomationTemplateInScope(instance.keyTemplate, scope(commentEvent("@engrams review").raw, "issue_comment.created"))).toBe("acme/manual#9");
    expect(
      await renderAutomationTemplateInScope(
        instance.keyTemplate,
        scope({ repository: { full_name: "acme/repo" }, pull_request: { number: 7, html_url: "https://github.com/acme/repo/pull/7" } }, "review.dispatch"),
      ),
    ).toBe("acme/repo#7");

    const closed = PR_REVIEW_DEFINITION.entrypoints!.find((ep) => ep.id === "closed")!;
    expect(closed.trigger).toMatchObject({ kind: "integration", provider: "github", eventKeys: ["pull_request.closed"] });
    expect(closed.blocks.map((b) => b.type)).toEqual(["instance_close"]);
    // A close only ends a workstream that exists.
    expect(instance.entrypoints).toEqual({ closed: { admit: "require" } });
  });

  describe("admission before the concurrency claim", () => {
    const verdict = (event: { raw: Record<string, unknown> }, eventKey: string) =>
      evaluateAdmissionPrelude({
        definition: PR_REVIEW_DEFINITION,
        inputs,
        automationId: "a1",
        automationName: "review",
        trigger: { kind: "integration", receivedAt: "2026-08-21T00:00:00Z", eventKey, payload: event.raw },
        aliases: [],
        entrypointId: "main",
        code: makeCodeBlockRuntime(),
      });

    test("the prelude is the facts step and the admit filter", () => {
      expect(admissionPrelude(PR_REVIEW_DEFINITION.blocks).map((b) => b.id)).toEqual(["facts", "admit"]);
    });

    test("a comment that is no command is rejected, so it does not supersede the active review", async () => {
      expect(await verdict(commentEvent("LGTM"), "issue_comment.created")).toMatchObject({
        kind: "reject",
        blockId: "admit",
      });
    });

    test("a review command from a person who may not ask is rejected", async () => {
      expect(await verdict(commentEvent("@engrams review", "NONE"), "issue_comment.created")).toMatchObject({
        kind: "reject",
      });
    });

    test("a review command from a member is admitted", async () => {
      expect(await verdict(commentEvent("@engrams review"), "issue_comment.created")).toEqual({ kind: "admit" });
    });
  });

  test("a full preview decides admission and renders the blocks that read the facts", async () => {
    const preview = await previewDefinition({
      definition: PR_REVIEW_DEFINITION,
      inputs,
      automationId: "a1",
      automationName: "review",
      trigger: {
        kind: "integration",
        receivedAt: "2026-08-21T00:00:00Z",
        eventKey: "pull_request.opened",
        payload: prEvent("opened").raw,
      },
      aliases: [],
      code: makeCodeBlockRuntime(),
    });
    expect(preview.blocks.find((b) => b.blockId === "admit")?.filterPass).toBe(true);
    expect(preview.errors.filter((e) => e.message.includes("steps.facts"))).toEqual([]);
  });

  test("every tunable field exists in its block's config", () => {
    const walk = (blocks: typeof PR_REVIEW_DEFINITION.blocks) => {
      for (const b of blocks) {
        for (const f of b.tunable ?? []) {
          expect(Object.keys(b.config), `${b.id}.${f}`).toContain(f);
        }
        if (b.then) walk(b.then);
        if (b.else) walk(b.else);
        if (b.body) walk(b.body);
      }
    };
    walk(PR_REVIEW_DEFINITION.blocks);
  });

  test("a block override on a tunable field merges and re-validates", () => {
    const merged = applyBlockOverrides(PR_REVIEW_DEFINITION, {
      find: { deadlineSeconds: 900 },
      finder: { networkOverride: { default: "deny", allowHosts: ["github.com", "pypi.org"], allowHostPatterns: [] } },
    });
    const find = merged.blocks.find((b) => b.id === "find")!;
    expect(find.config["deadlineSeconds"]).toBe(900);
    expect(() => applyBlockOverrides(PR_REVIEW_DEFINITION, { open: { repo: "x/y" } })).toThrow(/not tunable/);
  });

  test("defaultInputs matches the inputs schema keys", async () => {
    const defaults = await PR_REVIEW_BUILTIN.defaultInputs();
    expect(Object.keys(defaults).sort()).toEqual(
      PR_REVIEW_DEFINITION.inputsSchema.map((f) => f.key).sort(),
    );
  });
});

describe("review facts of a GitHub event", () => {
  const run = (event: { raw: Record<string, unknown> }, eventKey: string) =>
    reviewFacts(requireForge("github"), { eventKey, payload: event.raw, inputs });

  test("admits an auto-mode PR and derives the facts", async () => {
    expect(await run(prEvent("opened"), "pull_request.opened")).toMatchObject({
      admit: true,
      repo: "acme/repo",
      repo_name: "repo",
      pr_number: 7,
      trigger: "opened",
      head_sha: "a".repeat(40),
      pr_context: { providerId: "42", title: "Fix", additions: 1 },
      categories: ["functional-correctness"],
    });
  });

  test("rejects drafts, on_request repos for PR events, and unknown repos", async () => {
    expect(await run(prEvent("opened", { draft: true }), "pull_request.opened")).toBeNull();
    const manual = { raw: { ...prEvent("opened").raw, repository: { full_name: "acme/manual", name: "manual" } } };
    expect(await run(manual, "pull_request.opened")).toBeNull();
    const unknown = { raw: { ...prEvent("opened").raw, repository: { full_name: "other/repo", name: "repo" } } };
    expect(await run(unknown, "pull_request.opened")).toBeNull();
  });

  test("admits a member's review command on a mapped repo; rejects bots, outsiders, other text", async () => {
    expect(await run(commentEvent("@engrams review please"), "issue_comment.created")).toMatchObject({
      admit: true,
      trigger: "command",
      pr_number: 9,
    });
    expect(await run(commentEvent("@engrams review", "MEMBER", "Bot"), "issue_comment.created")).toBeNull();
    expect(await run(commentEvent("@engrams review", "NONE"), "issue_comment.created")).toBeNull();
    expect(await run(commentEvent("nice work @engrams"), "issue_comment.created")).toBeNull();
  });
});

describe("defaultMentionHandle", () => {
  test("derives from the App login, tolerating @ and [bot]; blank keeps the placeholder", () => {
    expect(defaultMentionHandle("engrams-agent")).toBe("@engrams-agent");
    expect(defaultMentionHandle("@engrams-agent[bot]")).toBe("@engrams-agent");
    expect(defaultMentionHandle("  ")).toBe("@engrams");
    expect(defaultMentionHandle("")).toBe("@engrams");
  });
});
