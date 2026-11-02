import { afterEach, expect, test, vi } from "vitest";
import { act, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient } from "@tanstack/react-query";
import { Code, ConnectError, createRouterTransport } from "@connectrpc/connect";
import { IntegrationService, McpCompletionSettlement } from "@/gen/engram/app/v1/integration_pb";
import { renderWithProviders } from "@/test-utils";
import { McpSessionPanel } from "./McpSessionPanel";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

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
        const bindingReceipt = { bindingHash: "H1", bindingEpoch: "1" };
        if (req.omitExecutions)
          return {
            bindingReceipt,
            bindingTools: [{ modelToolName: "tool-a", connectionId: "c1" }],
          };
        if (req.omitBindings)
          return {
            bindingReceipt,
            executions: [{ executionId: "e2", upstreamName: "second", state: "failed" }],
          };
        return {
          availability: "available",
          bindingReceipt,
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

test("result expansion shows persisted JSON as inert text and reports ambiguous settlement", async () => {
  let requests = 0;
  const fetch = vi.spyOn(globalThis, "fetch");
  const json =
    '{"outcome":"failed","delivery":"available","output":{"content":[{"type":"text","text":"<img src=https://example.test/x>"}],"structuredContent":{"value":1},"isError":true}}';
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: () => ({
        availability: "available",
        executions: [{ executionId: "e1", state: "failed", upstreamName: "write" }],
      }),
      getMcpExecutionResult: (req) => {
        expect(req.sessionId).toBe("s1");
        expect(req.executionId).toBe("e1");
        requests++;
        return {
          state: "failed",
          resultJson: json,
          settlement: McpCompletionSettlement.SETTLED_OR_TARGET_GONE,
        };
      },
    }),
  );
  const { container } = renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  expect(requests).toBe(0);
  await userEvent.click(await screen.findByRole("button", { name: "Show result" }));
  expect(await screen.findByText(json)).toBeTruthy();
  expect(screen.getByText(/settled or the target was unavailable/)).toBeTruthy();
  expect(container.querySelector("img, iframe, a[href^='https://example.test']")).toBeNull();
  expect(requests).toBe(1);
  expect(fetch).not.toHaveBeenCalled();
});

test("a base refetch clears supplemental tools and call actions before showing the new binding", async () => {
  let generation = 1;
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 0 } } });
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: (req) => {
        const bindingReceipt = {
          bindingHash: `H${generation}`,
          bindingEpoch: `${generation}`,
          current: true,
        };
        if (req.omitExecutions)
          return { bindingReceipt, bindingTools: [{ modelToolName: "old-extra-tool" }] };
        if (req.omitBindings)
          return {
            bindingReceipt,
            executions: [
              {
                executionId: "old-call",
                upstreamName: "old-extra-call",
                state: "dispatching",
                allowedActions: ["cancel"],
              },
            ],
          };
        return {
          bindingReceipt,
          bindingTools: [{ modelToolName: `base-tool-${generation}` }],
          hasMoreBindingTools: generation === 1,
          hasMoreExecutions: generation === 1,
          nextExecutionId: "first",
        };
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport, queryClient: client });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.click(await screen.findByRole("button", { name: "Load more tools" }));
  await screen.findByText("old-extra-tool");
  await userEvent.click(screen.getByRole("button", { name: "Load more calls" }));
  await screen.findByText("old-extra-call");
  generation = 2;
  await act(async () => {
    await client.invalidateQueries();
  });
  expect(await screen.findByText("base-tool-2")).toBeTruthy();
  expect(screen.queryByText("old-extra-tool")).toBeNull();
  expect(screen.queryByText("old-extra-call")).toBeNull();
  expect(screen.queryByRole("button", { name: "Cancel call" })).toBeNull();
});

test("a page from a different binding restarts pagination without displaying that page", async () => {
  let generation = 1;
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: (req) => {
        if (req.omitExecutions) {
          generation = 2;
          return {
            bindingReceipt: { bindingHash: "H2", bindingEpoch: "2" },
            bindingTools: [{ modelToolName: "mismatched-tool" }],
          };
        }
        return {
          bindingReceipt: { bindingHash: `H${generation}`, bindingEpoch: `${generation}` },
          bindingTools: [{ modelToolName: `base-tool-${generation}` }],
          hasMoreBindingTools: generation === 1,
        };
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.click(await screen.findByRole("button", { name: "Load more tools" }));
  expect(await screen.findByText("base-tool-2")).toBeTruthy();
  expect(screen.queryByText("mismatched-tool")).toBeNull();
});

test("a late older execution page cannot reappear after the base refresh", async () => {
  let generation = 1;
  let finishPage!: () => void;
  const pending = new Promise<void>((resolve) => {
    finishPage = resolve;
  });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 0 } } });
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: async (req) => {
        if (req.omitBindings) {
          await pending;
          return {
            bindingReceipt: { bindingHash: "H1", bindingEpoch: "1" },
            executions: [
              {
                executionId: "old",
                upstreamName: "late-old-call",
                state: "dispatching",
                allowedActions: ["cancel"],
              },
            ],
          };
        }
        return {
          bindingReceipt: { bindingHash: `H${generation}`, bindingEpoch: `${generation}` },
          bindingTools: [{ modelToolName: `base-tool-${generation}` }],
          hasMoreExecutions: generation === 1,
          nextExecutionId: "first",
        };
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport, queryClient: client });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.click(await screen.findByRole("button", { name: "Load more calls" }));
  generation = 2;
  await act(async () => {
    await client.invalidateQueries();
  });
  expect(await screen.findByText("base-tool-2")).toBeTruthy();
  await act(async () => {
    finishPage();
    await pending;
  });
  expect(screen.queryByText("late-old-call")).toBeNull();
  expect(screen.queryByRole("button", { name: "Cancel call" })).toBeNull();
});

test("a changed binding epoch in an execution page restarts the base read", async () => {
  let epoch = "1";
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: (req) => {
        if (req.omitBindings) {
          epoch = "2";
          return {
            bindingReceipt: { bindingHash: "same-hash", bindingEpoch: epoch },
            executions: [
              {
                executionId: "wrong",
                upstreamName: "wrong-call",
                state: "dispatching",
                allowedActions: ["cancel"],
              },
            ],
          };
        }
        return {
          bindingReceipt: { bindingHash: "same-hash", bindingEpoch: epoch },
          bindingTools: [{ modelToolName: `epoch-${epoch}` }],
          hasMoreExecutions: epoch === "1",
          nextExecutionId: "first",
        };
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.click(await screen.findByRole("button", { name: "Load more calls" }));
  expect(await screen.findByText("epoch-2")).toBeTruthy();
  expect(screen.queryByText("wrong-call")).toBeNull();
  expect(screen.queryByRole("button", { name: "Cancel call" })).toBeNull();
});

test("even an identical base response invalidates a late earlier page", async () => {
  let finishPage!: () => void;
  const pending = new Promise<void>((resolve) => {
    finishPage = resolve;
  });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 0 } } });
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: async (req) => {
        if (req.omitExecutions) {
          await pending;
          return {
            bindingReceipt: { bindingHash: "H1", bindingEpoch: "1" },
            bindingTools: [{ modelToolName: "late-tool" }],
          };
        }
        return {
          bindingReceipt: { bindingHash: "H1", bindingEpoch: "1" },
          bindingTools: [{ modelToolName: "base-tool" }],
          hasMoreBindingTools: true,
        };
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport, queryClient: client });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.click(await screen.findByRole("button", { name: "Load more tools" }));
  await act(async () => {
    await client.invalidateQueries();
  });
  await act(async () => {
    finishPage();
    await pending;
  });
  expect(screen.queryByText("late-tool")).toBeNull();
});

test("receiptless execution history remains available but receiptless tool pages are not shown", async () => {
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: (req) => {
        if (req.omitBindings)
          return {
            executions: [{ executionId: "history", upstreamName: "history-call", state: "failed" }],
          };
        if (req.omitExecutions) return { bindingTools: [{ modelToolName: "unverified-tool" }] };
        return { hasMoreExecutions: true, hasMoreBindingTools: true, nextExecutionId: "first" };
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.click(await screen.findByRole("button", { name: "Load more calls" }));
  expect(await screen.findByText("history-call")).toBeTruthy();
  await userEvent.click(screen.getByRole("button", { name: "Load more tools" }));
  expect(await screen.findByText(/binding changed or is unavailable/)).toBeTruthy();
  expect(screen.queryByText("unverified-tool")).toBeNull();
});

test("a transition from a receiptless base to a bound history page restarts the read", async () => {
  let bound = false;
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: (req) => {
        if (req.omitBindings) {
          bound = true;
          return {
            bindingReceipt: { bindingHash: "H2", bindingEpoch: "2" },
            executions: [
              {
                executionId: "new",
                upstreamName: "unmatched-call",
                state: "dispatching",
                allowedActions: ["cancel"],
              },
            ],
          };
        }
        return bound
          ? {
              bindingReceipt: { bindingHash: "H2", bindingEpoch: "2" },
              bindingTools: [{ modelToolName: "new-base" }],
            }
          : { hasMoreExecutions: true, nextExecutionId: "first" };
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.click(await screen.findByRole("button", { name: "Load more calls" }));
  expect(await screen.findByText("new-base")).toBeTruthy();
  expect(screen.queryByText("unmatched-call")).toBeNull();
  expect(screen.queryByRole("button", { name: "Cancel call" })).toBeNull();
});

test("a transition from a bound base to receiptless history restarts the read", async () => {
  let bound = true;
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: (req) => {
        if (req.omitBindings) {
          bound = false;
          return {
            executions: [
              {
                executionId: "old",
                upstreamName: "unmatched-call",
                state: "dispatching",
                allowedActions: ["cancel"],
              },
            ],
          };
        }
        return bound
          ? {
              bindingReceipt: { bindingHash: "H1", bindingEpoch: "1" },
              hasMoreExecutions: true,
              nextExecutionId: "first",
            }
          : { availability: "unavailable" };
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.click(await screen.findByRole("button", { name: "Load more calls" }));
  expect(await screen.findByText("Binding unavailable.")).toBeTruthy();
  expect(screen.queryByText("unmatched-call")).toBeNull();
  expect(screen.queryByRole("button", { name: "Cancel call" })).toBeNull();
});

test("failed base refetch invalidates previously loaded pages even if cached data is restored", async () => {
  let fail = false;
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 0 } } });
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: (req) => {
        if (req.omitExecutions)
          return {
            bindingReceipt: { bindingHash: "H1", bindingEpoch: "1" },
            bindingTools: [{ modelToolName: "old-extra-tool" }],
          };
        if (fail) throw new ConnectError("unavailable", Code.Unavailable);
        return {
          bindingReceipt: { bindingHash: "H1", bindingEpoch: "1" },
          hasMoreBindingTools: true,
        };
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport, queryClient: client });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.click(await screen.findByRole("button", { name: "Load more tools" }));
  await screen.findByText("old-extra-tool");
  const query = client
    .getQueryCache()
    .getAll()
    .find((q) => q.queryKey[0] === "connect-query")!;
  const previous = query.state.data;
  fail = true;
  await act(async () => {
    await client.invalidateQueries();
  });
  expect(await screen.findByText("Session MCP state is unavailable.")).toBeTruthy();
  await act(async () => {
    client.setQueryData(query.queryKey, previous);
  });
  expect(screen.queryByText("old-extra-tool")).toBeNull();
});

test("a page started before a failed base refetch cannot reappear after cache recovery", async () => {
  let fail = false;
  let finishPage!: () => void;
  const pending = new Promise<void>((resolve) => {
    finishPage = resolve;
  });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 0 } } });
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: async (req) => {
        if (req.omitBindings) {
          await pending;
          return {
            bindingReceipt: { bindingHash: "H1", bindingEpoch: "1" },
            executions: [
              {
                executionId: "old",
                upstreamName: "late-old-call",
                state: "dispatching",
                allowedActions: ["cancel"],
              },
            ],
          };
        }
        if (fail) throw new ConnectError("unavailable", Code.Unavailable);
        return {
          bindingReceipt: { bindingHash: "H1", bindingEpoch: "1" },
          hasMoreExecutions: true,
          nextExecutionId: "first",
        };
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport, queryClient: client });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  await userEvent.click(await screen.findByRole("button", { name: "Load more calls" }));
  const query = client
    .getQueryCache()
    .getAll()
    .find((q) => q.queryKey[0] === "connect-query")!;
  const previous = query.state.data;
  fail = true;
  await act(async () => {
    await client.invalidateQueries();
  });
  expect(await screen.findByText("Session MCP state is unavailable.")).toBeTruthy();
  await act(async () => {
    finishPage();
    await pending;
  });
  await act(async () => {
    client.setQueryData(query.queryKey, previous);
  });
  expect(screen.queryByText("late-old-call")).toBeNull();
  expect(screen.queryByRole("button", { name: "Cancel call" })).toBeNull();
});

test("pagination cannot start during a base refetch that fails and restores cached data", async () => {
  let failRead!: (error: Error) => void;
  let reads = 0;
  let pageRequests = 0;
  const pending = new Promise<never>((_resolve, reject) => {
    failRead = reject;
  });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 0 } } });
  const transport = createRouterTransport((router) =>
    router.service(IntegrationService, {
      getSessionMcpState: (req) => {
        if (req.omitExecutions || req.omitBindings) {
          pageRequests++;
          return {
            bindingReceipt: { bindingHash: "H1", bindingEpoch: "1" },
            bindingTools: [{ modelToolName: "wrong-tool" }],
            executions: [
              {
                executionId: "old",
                upstreamName: "wrong-call",
                state: "dispatching",
                allowedActions: ["cancel"],
              },
            ],
          };
        }
        reads++;
        if (reads > 1) return pending;
        return {
          bindingReceipt: { bindingHash: "H1", bindingEpoch: "1" },
          hasMoreBindingTools: true,
          hasMoreExecutions: true,
          nextExecutionId: "first",
        };
      },
    }),
  );
  renderWithProviders(<McpSessionPanel sessionId="s1" />, { transport, queryClient: client });
  await userEvent.click(await screen.findByText("Session MCP tools"));
  const tools = await screen.findByRole("button", { name: "Load more tools" });
  const calls = screen.getByRole("button", { name: "Load more calls" });
  const query = client
    .getQueryCache()
    .getAll()
    .find((q) => q.queryKey[0] === "connect-query")!;
  const previous = query.state.data;
  let refetch!: Promise<void>;
  await act(async () => {
    refetch = client.invalidateQueries();
    // These events reach the old render before React applies the fetch notification.
    fireEvent.click(tools);
    fireEvent.click(calls);
  });
  expect((tools as HTMLButtonElement).disabled).toBe(true);
  expect((calls as HTMLButtonElement).disabled).toBe(true);
  expect(pageRequests).toBe(0);
  await act(async () => {
    failRead(new ConnectError("unavailable", Code.Unavailable));
    await refetch;
  });
  await screen.findByText("Session MCP state is unavailable.");
  await act(async () => {
    client.setQueryData(query.queryKey, previous);
  });
  expect(screen.queryByText("wrong-tool")).toBeNull();
  expect(screen.queryByText("wrong-call")).toBeNull();
  expect(screen.queryByRole("button", { name: "Cancel call" })).toBeNull();
});
