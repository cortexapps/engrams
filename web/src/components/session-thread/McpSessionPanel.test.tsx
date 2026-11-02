import { afterEach, expect, test } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Code, ConnectError, createRouterTransport } from "@connectrpc/connect";
import { IntegrationService } from "@/gen/engram/app/v1/integration_pb";
import { renderWithProviders } from "@/test-utils";
import { McpSessionPanel } from "./McpSessionPanel";

afterEach(cleanup);

test("approval uses the original arguments and exact server review context", async () => {
  let request: unknown;
  const context = {
    executionId: "e1",
    sessionId: "s1",
    requestDigest: "digest",
    argumentHash: "hash",
    contextDigest: "context",
  };
  const transport = createRouterTransport((router) => {
    router.service(IntegrationService, {
      getSessionMcpState: () => ({
        availability: "available",
        executions: [
          {
            executionId: "e1",
            state: "waiting_approval",
            allowedActions: ["approve"],
            connectionId: "c1",
            upstreamName: "write",
          },
        ],
        approvals: [
          {
            executionId: "e1",
            connectionId: "c1",
            upstreamName: "write",
            originalArguments: '{"original":true}',
            expiresAt: "2099-01-01T00:00:00Z",
            expectedReviewContext: context,
          },
        ],
      }),
      decideMcpApproval: (req) => {
        request = req;
        return {};
      },
    });
  });
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  expect(await screen.findByText('{"original":true}')).toBeTruthy();
  await userEvent.click(screen.getByRole("button", { name: "Approve" }));
  expect(request).toMatchObject({
    executionId: "e1",
    approve: true,
    expectedReviewContext: context,
  });
});

test("cancellation and unknown resolution send the current journal context", async () => {
  let canceled: unknown;
  let resolved: unknown;
  const unknown = {
    executionId: "e2",
    journalRevision: 5,
    requestDigest: "digest",
    bindingHash: "binding",
    intentId: "intent",
  };
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: () => ({
        availability: "unavailable",
        unknownWriteBlocks: ["c1"],
        executions: [
          {
            executionId: "e1",
            state: "dispatching",
            journalRevision: 3,
            allowedActions: ["cancel"],
            connectionId: "c1",
            upstreamName: "write",
          },
          {
            executionId: "e2",
            state: "outcome_unknown",
            expectedUnknownContext: unknown,
            allowedActions: ["resolve_unknown"],
            connectionId: "c1",
            upstreamName: "write",
          },
        ],
      }),
      cancelMcpCall: (req) => {
        canceled = req;
        return {};
      },
      resolveMcpOutcome: (req) => {
        resolved = req;
        return {};
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  expect(await screen.findByText(/New calls for connection c1 are blocked/)).toBeTruthy();
  await userEvent.click(screen.getByRole("button", { name: "Cancel call" }));
  expect(canceled).toMatchObject({ executionId: "e1", expectedJournalRevision: 3 });
  expect(await screen.findByText(/Cancellation requested/)).toBeTruthy();
  await userEvent.selectOptions(screen.getByLabelText("Observation"), "acknowledged");
  await userEvent.click(screen.getByRole("button", { name: "Record observation" }));
  expect(resolved).toMatchObject({
    executionId: "e2",
    outcome: "acknowledged",
    expectedUnknownContext: unknown,
  });
});

test("rebind requires review and submits both server hashes", async () => {
  let request: unknown;
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: () => ({
        availability: "unavailable",
        bindingReceipt: {
          bindingHash: "old",
          bindingEpoch: "1",
          staleReasons: ["unavailable"],
          allowedActions: ["rebind"],
        },
      }),
      prepareMcpRebind: () => ({
        bindingHash: "candidate",
        bindingEpoch: "2",
        connections: [{ connectionId: "c1", alias: "work", endpoint: "https://example.test" }],
      }),
      rebindMcp: (req) => {
        request = req;
        return {};
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.click(await screen.findByRole("button", { name: "Review rebind" }));
  expect(await screen.findByText(/Candidate binding candidate/)).toBeTruthy();
  expect(request).toBeUndefined();
  await userEvent.click(screen.getByRole("button", { name: "Confirm rebind" }));
  expect(request).toMatchObject({
    sessionId: "s1",
    expectedBindingHash: "old",
    expectedCandidateHash: "candidate",
  });
});

test("unavailable state and absent server actions do not offer decisions", async () => {
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: () => ({
        availability: "unavailable",
        executions: [
          {
            executionId: "e1",
            state: "waiting_approval",
            allowedActions: [],
            reason: "review_expired_or_authority_unavailable",
          },
        ],
      }),
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  expect(await screen.findByText("review_expired_or_authority_unavailable")).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Approve" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Record observation" })).toBeNull();
});

test("a stale or revoked approval reloads state without another decision", async () => {
  let reads = 0;
  let decisions = 0;
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: () => {
        reads++;
        return reads === 1
          ? {
              availability: "available",
              executions: [
                { executionId: "e1", state: "waiting_approval", allowedActions: ["approve"] },
              ],
              approvals: [
                {
                  executionId: "e1",
                  originalArguments: "{}",
                  expectedReviewContext: { executionId: "e1" },
                },
              ],
            }
          : {
              availability: "unavailable",
              executions: [
                {
                  executionId: "e1",
                  state: "waiting_approval",
                  reason: "review_expired_or_authority_unavailable",
                },
              ],
            };
      },
      decideMcpApproval: () => {
        decisions++;
        throw new ConnectError("permission changed", Code.PermissionDenied);
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.click(await screen.findByRole("button", { name: "Approve" }));
  expect(await screen.findByText(/request could not be completed/)).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Approve" })).toBeNull();
  await waitFor(() => expect(reads).toBeGreaterThan(1));
  expect(decisions).toBe(1);
});

test("execution and tool pages advance independently when the first tool page is empty", async () => {
  const requests: {
    omitBindings: boolean;
    omitExecutions: boolean;
    afterBindingToolName: string;
    afterExecutionId: string;
  }[] = [];
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: (req) => {
        requests.push(req);
        if (req.omitExecutions)
          return { bindingTools: [{ modelToolName: "tool-a", connectionId: "c1" }] };
        if (req.omitBindings)
          return { executions: [{ executionId: "e2", upstreamName: "second", state: "failed" }] };
        return {
          availability: "available",
          hasMoreBindingTools: true,
          hasMoreExecutions: true,
          nextExecutionId: "e1",
          executions: [{ executionId: "e1", upstreamName: "first", state: "failed" }],
        };
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.click(await screen.findByRole("button", { name: "Load more tools" }));
  expect(await screen.findByText("tool-a")).toBeTruthy();
  await userEvent.click(screen.getByRole("button", { name: "Load more calls" }));
  expect(await screen.findByText("second")).toBeTruthy();
  expect(requests).toEqual(
    expect.arrayContaining([
      expect.objectContaining({ omitExecutions: true, afterBindingToolName: "" }),
      expect.objectContaining({ omitBindings: true, afterExecutionId: "e1" }),
    ]),
  );
});

test("a late result prevents stale resolution and refreshes the execution", async () => {
  let reads = 0;
  let resolutions = 0;
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: () => {
        reads++;
        return reads === 1
          ? {
              availability: "available",
              executions: [
                {
                  executionId: "e1",
                  state: "outcome_unknown",
                  allowedActions: ["resolve_unknown"],
                  expectedUnknownContext: {
                    executionId: "e1",
                    journalRevision: 1,
                    intentId: "intent",
                  },
                },
              ],
            }
          : {
              availability: "available",
              executions: [{ executionId: "e1", state: "succeeded", resultUnavailable: true }],
            };
      },
      resolveMcpOutcome: () => {
        resolutions++;
        throw new ConnectError("stale context", Code.FailedPrecondition);
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.selectOptions(await screen.findByLabelText("Observation"), "succeeded");
  await userEvent.click(screen.getByRole("button", { name: "Record observation" }));
  expect(await screen.findByText(/Result unavailable/)).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Record observation" })).toBeNull();
  expect(resolutions).toBe(1);
});
