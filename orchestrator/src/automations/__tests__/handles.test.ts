/** Handle-candidate extraction (ADR 0120 instances), driven by the REAL
 * built-in facets and their checked-in event samples — the same fixtures the
 * picker tests pin — so a manifest edit that breaks routing fails here, not
 * in prod admission. */

import { describe, expect, test } from "bun:test";

import { connectorRegistry } from "../../connectors/registry.ts";
import { loadEventSample } from "../../connectors/samples.ts";
import {
  canonicalHandle,
  extractHandleCandidates,
  renderHandleParts,
} from "../handles.ts";

const registry = connectorRegistry();
const slack = registry.get("slack")!;
const github = registry.get("github")!;

function sample(provider: string, eventKey: string): Record<string, unknown> {
  const payload = loadEventSample(provider, eventKey);
  if (!payload) throw new Error(`missing sample ${provider}/${eventKey}`);
  return payload;
}

describe("renderHandleParts", () => {
  const parts = [{ lit: "slack:" }, { path: "channel" }, { lit: ":" }, { path: "ts" }];

  test("concatenates literals and scalar path values", () => {
    const scope: Record<string, unknown> = { channel: "C1", ts: 1724500000.0001 };
    expect(renderHandleParts(parts, (p) => scope[p])).toBe("slack:C1:1724500000.0001");
  });

  test("a missing, empty, or non-scalar path value kills the candidate", () => {
    for (const bad of [undefined, "", true, { deep: 1 }, ["x"], null, Number.NaN]) {
      const scope: Record<string, unknown> = { channel: "C1", ts: bad };
      expect(renderHandleParts(parts, (p) => scope[p])).toBeNull();
    }
  });
});

describe("canonicalHandle", () => {
  test("github handles fold case; slack handles stay exact", () => {
    expect(canonicalHandle("github", "github:Acme/Engrams#41")).toBe("github:acme/engrams#41");
    expect(canonicalHandle("slack", "slack:C0AAAAAAA:1.2")).toBe("slack:C0AAAAAAA:1.2");
  });
});

describe("extractHandleCandidates (built-in facets over their samples)", () => {
  test("a slack thread reply yields thread-then-channel; a top-level message yields channel only", () => {
    // Declaration order IS routing precedence (rung 2): the thread handle
    // comes first, so a thread owned by workstream A wins over the channel
    // owned by workstream B; a top-level message routes by the channel.
    const payload = sample("slack", "message");
    expect(
      extractHandleCandidates({
        provider: "slack",
        facet: slack.webhook,
        eventKey: "message",
        payload,
      }),
    ).toEqual(["slack:C0AAAAAAA:1755763100.000100", "slack:C0AAAAAAA"]);

    const topLevel = structuredClone(payload);
    delete (topLevel.event as Record<string, unknown>).thread_ts;
    expect(
      extractHandleCandidates({
        provider: "slack",
        facet: slack.webhook,
        eventKey: "message",
        payload: topLevel,
      }),
    ).toEqual(["slack:C0AAAAAAA"]);
  });

  test("a reaction routes by the reacted message's identity, then its channel", () => {
    expect(
      extractHandleCandidates({
        provider: "slack",
        facet: slack.webhook,
        eventKey: "reaction_added",
        payload: sample("slack", "reaction_added"),
      }),
    ).toEqual(["slack:C0AAAAAAA:1755763260.000200", "slack:C0AAAAAAA"]);
  });

  test("github PR events and PR-comment events agree on the repo#number handle, case-folded", () => {
    const pr = structuredClone(sample("github", "pull_request.opened"));
    (pr.repository as Record<string, unknown>).full_name = "Acme/Engrams";
    expect(
      extractHandleCandidates({
        provider: "github",
        facet: github.webhook,
        eventKey: "pull_request.opened",
        payload: pr,
      }),
    ).toEqual(["github:acme/engrams#41"]);

    // issue_comment on a PR carries the same number under issue.number — the
    // two events must resolve to the SAME handle or feedback routing splits.
    expect(
      extractHandleCandidates({
        provider: "github",
        facet: github.webhook,
        eventKey: "issue_comment.created",
        payload: sample("github", "issue_comment.created"),
      }),
    ).toEqual(["github:acme/engrams#73"]);
  });

  test("events with no declaration, unknown keys, and a missing facet yield nothing", () => {
    expect(
      extractHandleCandidates({
        provider: "github",
        facet: github.webhook,
        eventKey: "push",
        payload: sample("github", "push"),
      }),
    ).toEqual([]);
    expect(
      extractHandleCandidates({
        provider: "github",
        facet: github.webhook,
        eventKey: "not.declared",
        payload: {},
      }),
    ).toEqual([]);
    expect(
      extractHandleCandidates({ provider: "x", facet: undefined, eventKey: "e", payload: {} }),
    ).toEqual([]);
  });

  test("duplicate renders dedupe (declaration order wins)", () => {
    const facet = {
      events: [
        {
          key: "e",
          label: "E",
          handleCandidates: [
            { parts: [{ lit: "p:" }, { path: "a" }] },
            { parts: [{ lit: "p:" }, { path: "b" }] },
            { parts: [{ lit: "p:" }, { path: "c" }] },
          ],
        },
      ],
    };
    expect(
      extractHandleCandidates({
        provider: "x",
        facet,
        eventKey: "e",
        payload: { a: "1", b: "1", c: "2" },
      }),
    ).toEqual(["p:1", "p:2"]);
  });

  test("every built-in slack/github action handle declaration parses against its own schema", () => {
    // The registry parse already enforced input./output. membership; pin the
    // seeds that PR inst/04's executor will render.
    const post = slack.actions!.find((a) => a.id === "post_message")!;
    expect(post.handles?.map((t) => t.parts)).toEqual([
      [{ lit: "slack:" }, { path: "output.channel" }, { lit: ":" }, { path: "input.threadTs" }],
      [{ lit: "slack:" }, { path: "output.channel" }, { lit: ":" }, { path: "output.ts" }],
    ]);
    expect(github.actions!.find((a) => a.id === "post_pr_review")!.handles).toHaveLength(1);
    expect(github.actions!.find((a) => a.id === "create_issue_comment")!.handles).toHaveLength(1);
  });
});
