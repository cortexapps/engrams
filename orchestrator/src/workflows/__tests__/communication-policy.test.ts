/**
 * Framework event-classification (ADR 0059 P1.4).
 *
 * `routeSessionEvent` is the pure decision the thread workflow makes for each
 * curated session event: which `CommunicationPolicy` method to invoke (and,
 * for questions, the `tool_call_id` correlation token). Pure → unit-tested
 * here in isolation; the workflow just executes the chosen effect as a step.
 */

import { expect, test, describe } from "bun:test";
import { routeSessionEvent, summarizeAsset } from "../communication-policy.ts";
import type { CuratedEvent } from "../../control-plane/session-events.ts";

const ev = (kind: string, payloadJson = "{}"): CuratedEvent => ({ idx: 0n, kind, payloadJson });

describe("routeSessionEvent()", () => {
  test("user_question → question, carrying the tool_call_id", () => {
    expect(routeSessionEvent(ev("user_question", '{"tool_call_id":"t1"}'))).toEqual({
      kind: "question",
      toolCallId: "t1",
    });
  });

  test("question_answered → answered, carrying the tool_call_id", () => {
    expect(routeSessionEvent(ev("question_answered", '{"tool_call_id":"t1"}'))).toEqual({
      kind: "answered",
      toolCallId: "t1",
    });
  });

  test("integration_asset and file_shared → asset", () => {
    expect(routeSessionEvent(ev("integration_asset"))).toEqual({ kind: "asset" });
    expect(routeSessionEvent(ev("file_shared"))).toEqual({ kind: "asset" });
  });

  test("run_started / run_completed → ignore (curated but not rendered as content)", () => {
    expect(routeSessionEvent(ev("run_started"))).toEqual({ kind: "ignore" });
    // run_completed is NOT terminal (Invariant 2) and renders nothing.
    expect(routeSessionEvent(ev("run_completed"))).toEqual({ kind: "ignore" });
  });

  test("a question with an unparseable payload still routes (toolCallId undefined)", () => {
    expect(routeSessionEvent(ev("user_question", "not json"))).toEqual({
      kind: "question",
      toolCallId: undefined,
    });
  });
});

// summarizeAsset collapses a curated asset event into a one-line label (+ link)
// for the closing-summary recap (ADR 0059 onComplete). Pure; only DURABLE
// assets count — a transient `surface:"action"` (a query the agent ran) is not
// a recap line (matches the ADR 0056 asset-vs-action distinction).
describe("summarizeAsset()", () => {
  const asset = (data: Record<string, unknown>) => ev("integration_asset", JSON.stringify(data));

  test("a pull_request → 'PR #<n>: <title>' with the external link", () => {
    expect(
      summarizeAsset(
        asset({
          provider: "forge",
          asset_kind: "pull_request",
          surface: "asset",
          data: { number: 42, title: "Fix the bug", repo: "acme/app" },
          fetchable: { kind: "external", url: "https://github.com/acme/app/pull/42" },
        }),
      ),
    ).toEqual({ label: "PR #42: Fix the bug", url: "https://github.com/acme/app/pull/42" });
  });

  test("a transient action (surface:'action') is NOT a recap asset → null", () => {
    expect(
      summarizeAsset(
        asset({ provider: "forge", asset_kind: "search", surface: "action", data: {} }),
      ),
    ).toBeNull();
  });

  test("a generic durable asset → '<provider> <asset_kind>' + link when present", () => {
    expect(
      summarizeAsset(
        asset({
          provider: "linear",
          asset_kind: "issue",
          surface: "asset",
          data: {},
          fetchable: { kind: "external", url: "https://linear.app/x/issue/ENG-1" },
        }),
      ),
    ).toEqual({ label: "linear issue", url: "https://linear.app/x/issue/ENG-1" });
  });

  test("a file_shared → its caption, no external link", () => {
    expect(
      summarizeAsset(ev("file_shared", JSON.stringify({ artifact_id: "a1", caption: "screenshot.png" }))),
    ).toEqual({ label: "screenshot.png" });
  });

  test("a file_shared with no caption falls back to a generic label", () => {
    expect(summarizeAsset(ev("file_shared", JSON.stringify({ artifact_id: "a1" })))).toEqual({
      label: "shared a file",
    });
  });

  test("a malformed payload → null (never throws)", () => {
    expect(summarizeAsset(ev("integration_asset", "not json"))).toBeNull();
    expect(summarizeAsset(ev("file_shared", "{bad"))).toBeNull();
  });
});
