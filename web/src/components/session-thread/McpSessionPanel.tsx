import { createClient } from "@connectrpc/connect";
import { useQuery, useTransport } from "@connectrpc/connect-query";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { getSessionMcpState } from "@/gen/engram/app/v1/integration-IntegrationService_connectquery";
import {
  IntegrationService,
  McpCompletionSettlement,
  type GetMcpExecutionResultResponse,
  type McpBoundToolSummary,
  type PrepareMcpRebindResponse,
} from "@/gen/engram/app/v1/integration_pb";

function Tools({ tools }: { tools: McpBoundToolSummary[] }) {
  return tools.map((tool) => (
    <div key={tool.modelToolName} className="border-t py-2 text-xs">
      <p>
        <strong>{tool.modelToolName}</strong> · {tool.connectionAlias} ({tool.connectionId}) ·{" "}
        {tool.reviewedTool?.classification}
      </p>
      <p>
        Catalog {tool.catalogId} revision {tool.catalogRevision} · configuration{" "}
        {tool.configRevision} · authorization {tool.authGeneration}
      </p>
      <p>{tool.reviewedTool?.description}</p>
      <p>Fingerprint: {tool.reviewedTool?.fingerprint}</p>
      <p>
        Automation writes: {tool.automationWrite ? "allowed" : "not allowed"} ·{" "}
        {tool.unrestricted ? "unrestricted" : "constrained"}
      </p>
      {tool.argumentConstraintDocumentsJson.map((doc, i) => (
        <pre className="whitespace-pre-wrap break-all" key={i}>
          {doc}
        </pre>
      ))}
    </div>
  ));
}

function ExecutionResult({ sessionId, executionId }: { sessionId: string; executionId: string }) {
  const client = createClient(IntegrationService, useTransport());
  const [result, setResult] = useState<GetMcpExecutionResultResponse | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(false);
  async function load() {
    setBusy(true);
    setError(false);
    setResult(null);
    try {
      setResult(await client.getMcpExecutionResult({ sessionId, executionId }));
    } catch {
      setError(true);
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="flex flex-col gap-1">
      <Button size="sm" variant="outline" disabled={busy} onClick={() => void load()}>
        {result ? "Refresh result" : "Show result"}
      </Button>
      {error && (
        <p role="alert">Result is unavailable. The session or permission may have changed.</p>
      )}
      {result && (
        <>
          <p>Execution state: {result.state}</p>
          {result.resultUnavailable && <p>Result unavailable.</p>}
          <p>
            {result.settlement === McpCompletionSettlement.PENDING
              ? "Completion settlement is pending."
              : result.settlement === McpCompletionSettlement.SETTLED_OR_TARGET_GONE
                ? "Completion was settled or the target was unavailable. This does not confirm guest delivery."
                : result.settlement === McpCompletionSettlement.NOT_APPLICABLE
                  ? "Completion settlement is not applicable yet."
                  : "Completion settlement is unavailable."}
          </p>
          {result.resultJson !== undefined ? (
            <pre className="whitespace-pre-wrap break-all">{result.resultJson}</pre>
          ) : (
            <p>No stored result.</p>
          )}
        </>
      )}
    </div>
  );
}

export function McpSessionPanel({ sessionId }: { sessionId: string }) {
  const client = createClient(IntegrationService, useTransport());
  const [open, setOpen] = useState(false);
  const state = useQuery(
    getSessionMcpState,
    { sessionId, pageSize: 16, bindingPageSize: 16 },
    { enabled: open },
  );
  const [executionPages, setExecutionPages] = useState<NonNullable<typeof state.data>[]>([]);
  const [bindingPages, setBindingPages] = useState<NonNullable<typeof state.data>[]>([]);
  const [candidate, setCandidate] = useState<{
    review: PrepareMcpRebindResponse;
    expectedBindingHash: string;
  } | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const [observation, setObservation] = useState<Record<string, string>>({});
  const pages = state.data ? [state.data, ...executionPages] : [];
  const binding = state.data?.bindingReceipt;
  async function refresh() {
    setExecutionPages([]);
    setBindingPages([]);
    setCandidate(null);
    await state.refetch();
  }
  async function action(fn: () => Promise<unknown>, success: string) {
    setBusy(true);
    setMessage("");
    try {
      await fn();
      await refresh();
      setMessage(success);
    } catch {
      await refresh();
      setMessage("The request could not be completed. State may have changed; review it again.");
    } finally {
      setBusy(false);
    }
  }
  const proposal = candidate?.review;
  const lastExecution = executionPages.at(-1) ?? state.data;
  const lastBinding = bindingPages.at(-1) ?? state.data;
  return (
    <details
      className="max-h-[50vh] overflow-auto border-b px-4 py-2 text-sm"
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary>Session MCP tools</summary>
      {open && state.isPending && <p>Loading session MCP state…</p>}
      {state.isError && (
        <div>
          <p role="alert">Session MCP state is unavailable.</p>
          <Button size="sm" variant="ghost" disabled={busy} onClick={() => void refresh()}>
            Retry
          </Button>
        </div>
      )}
      {state.data && !state.isError && (
        <div className="flex flex-col gap-2 pt-2">
          <p>Availability: {state.data.availability || "unavailable"}</p>
          {binding ? (
            <p>
              Binding {binding.bindingHash} · epoch {binding.bindingEpoch} ·{" "}
              {binding.current ? "current" : "unavailable or stale"}{" "}
              {binding.staleReasons.join(", ")}
            </p>
          ) : (
            <p>Binding unavailable.</p>
          )}
          <p>
            Write policy: {state.data.writePolicy?.policy?.writeEligibility || "unavailable"}{" "}
            {state.data.writePolicy?.reason}
          </p>
          {state.data.unknownWriteBlocks.map((id) => (
            <p key={id}>
              New calls for connection {id} are blocked while the write outcome is unknown.
            </p>
          ))}
          <Tools
            tools={[...state.data.bindingTools, ...bindingPages.flatMap((p) => p.bindingTools)]}
          />
          {lastBinding?.hasMoreBindingTools && (
            <Button
              size="sm"
              variant="outline"
              disabled={busy}
              onClick={async () => {
                setBusy(true);
                try {
                  const page = await client.getSessionMcpState({
                    sessionId,
                    pageSize: 16,
                    bindingPageSize: 16,
                    omitExecutions: true,
                    afterBindingToolName: lastBinding.nextBindingToolName,
                  });
                  setBindingPages((old) => [...old, page]);
                } catch {
                  setMessage("Could not load more tools.");
                } finally {
                  setBusy(false);
                }
              }}
            >
              Load more tools
            </Button>
          )}
          {pages.flatMap((page) =>
            page.executions.map((execution) => {
              const review =
                page.approvals.find((a) => a.executionId === execution.executionId) ??
                execution.review;
              const receipt = page.resolutionReceipts.find(
                (r) => r.executionId === execution.executionId,
              );
              return (
                <section className="flex flex-col gap-1 border-t py-2" key={execution.executionId}>
                  <p>
                    <strong>{execution.upstreamName}</strong> · connection {execution.connectionId}{" "}
                    · {execution.state}
                  </p>
                  <ExecutionResult
                    key={`${execution.executionId}:${execution.journalRevision}`}
                    sessionId={sessionId}
                    executionId={execution.executionId}
                  />
                  {execution.reason && <p>{execution.reason}</p>}
                  {execution.approvalDecision && <p>Decision: {execution.approvalDecision}</p>}
                  {!review && execution.originalArguments && (
                    <pre className="whitespace-pre-wrap break-all">
                      {execution.originalArguments}
                    </pre>
                  )}
                  {execution.resultUnavailable && (
                    <p>Result unavailable. This does not establish the upstream outcome.</p>
                  )}
                  {review && (
                    <>
                      <p>
                        Expires {review.expiresAt} · catalog{" "}
                        {review.expectedReviewContext?.catalogId} revision{" "}
                        {review.expectedReviewContext?.catalogRevision} · configuration{" "}
                        {review.expectedReviewContext?.configRevision} · authorization{" "}
                        {review.expectedReviewContext?.authGeneration} · binding{" "}
                        {review.expectedReviewContext?.bindingHash} · epoch{" "}
                        {review.expectedReviewContext?.bindingEpoch} · tool {review.upstreamName} ·{" "}
                        {review.reviewedTool?.classification} · fingerprint{" "}
                        {review.expectedReviewContext?.toolFingerprint} · request{" "}
                        {review.requestDigest} · arguments {review.argumentHash}
                      </p>
                      <p>{review.reviewedTool?.description}</p>
                      <pre className="whitespace-pre-wrap break-all">
                        {review.originalArguments}
                      </pre>
                      {review.argumentConstraintDocumentsJson.map((doc, i) => (
                        <pre className="whitespace-pre-wrap break-all" key={i}>
                          {doc}
                        </pre>
                      ))}
                    </>
                  )}
                  {review?.expectedReviewContext &&
                    ["approve", "reject"]
                      .filter((a) => execution.allowedActions.includes(a))
                      .map((decision) => (
                        <Button
                          key={decision}
                          size="sm"
                          disabled={busy}
                          onClick={() =>
                            void action(
                              () =>
                                client.decideMcpApproval({
                                  executionId: execution.executionId,
                                  approve: decision === "approve",
                                  expectedReviewContext: review.expectedReviewContext,
                                }),
                              "Decision recorded.",
                            )
                          }
                        >
                          {decision === "approve" ? "Approve" : "Reject"}
                        </Button>
                      ))}
                  {execution.allowedActions.includes("cancel") && (
                    <Button
                      size="sm"
                      disabled={busy}
                      onClick={() =>
                        void action(
                          () =>
                            client.cancelMcpCall({
                              executionId: execution.executionId,
                              expectedJournalRevision: execution.journalRevision,
                            }),
                          "Cancellation requested. An external effect may already have occurred.",
                        )
                      }
                    >
                      Cancel call
                    </Button>
                  )}
                  {execution.cancellationReceipt && (
                    <p>Cancellation requested: {execution.cancellationReceipt.state}</p>
                  )}
                  {execution.allowedActions.includes("resolve_unknown") &&
                    execution.expectedUnknownContext && (
                      <>
                        <label>
                          Observation{" "}
                          <select
                            value={observation[execution.executionId] ?? ""}
                            onChange={(e) =>
                              setObservation({
                                ...observation,
                                [execution.executionId]: e.target.value,
                              })
                            }
                          >
                            <option value="">Select an observation</option>
                            <option value="succeeded">Succeeded</option>
                            <option value="failed">Failed</option>
                            <option value="acknowledged">Outcome remains unknown</option>
                          </select>
                        </label>
                        <p>Recording an observation does not undo or retry the external effect.</p>
                        <Button
                          size="sm"
                          disabled={busy || !observation[execution.executionId]}
                          onClick={() =>
                            void action(
                              () =>
                                client.resolveMcpOutcome({
                                  executionId: execution.executionId,
                                  outcome: observation[execution.executionId],
                                  expectedUnknownContext: execution.expectedUnknownContext,
                                }),
                              "Observation recorded.",
                            )
                          }
                        >
                          Record observation
                        </Button>
                      </>
                    )}
                  {receipt && (
                    <p>
                      Observation: {receipt.outcome} · {receipt.createdAt} · {receipt.actorId}
                    </p>
                  )}
                </section>
              );
            }),
          )}
          {lastExecution?.hasMoreExecutions && (
            <Button
              size="sm"
              variant="outline"
              disabled={busy}
              onClick={async () => {
                setBusy(true);
                try {
                  const page = await client.getSessionMcpState({
                    sessionId,
                    pageSize: 16,
                    bindingPageSize: 16,
                    omitBindings: true,
                    afterExecutionId: lastExecution.nextExecutionId,
                  });
                  setExecutionPages((old) => [...old, page]);
                } catch {
                  setMessage("Could not load more calls.");
                } finally {
                  setBusy(false);
                }
              }}
            >
              Load more calls
            </Button>
          )}
          {binding?.allowedActions.includes("rebind") && !candidate && (
            <Button
              size="sm"
              disabled={busy}
              onClick={async () => {
                setBusy(true);
                try {
                  setCandidate({
                    review: await client.prepareMcpRebind({
                      sessionId,
                      expectedBindingHash: binding.bindingHash,
                    }),
                    expectedBindingHash: binding.bindingHash,
                  });
                } catch {
                  await refresh();
                  setMessage("Could not prepare rebind. Review the latest state.");
                } finally {
                  setBusy(false);
                }
              }}
            >
              Review rebind
            </Button>
          )}
          {candidate && proposal && (
            <section>
              <p>
                Candidate binding {proposal.bindingHash} · epoch {proposal.bindingEpoch}
              </p>
              {proposal.connections.map((c) => (
                <p key={c.connectionId}>
                  {c.alias} · {c.endpoint} · {c.zone} · catalog {c.catalogRevision} · configuration{" "}
                  {c.configRevision} · authorization {c.authGeneration}
                </p>
              ))}
              <Tools tools={proposal.bindingTools} />
              {proposal.drain ? (
                <p>Rebind is waiting for active calls or an unknown outcome.</p>
              ) : (
                <Button
                  size="sm"
                  disabled={busy}
                  onClick={() =>
                    void action(
                      () =>
                        client.rebindMcp({
                          sessionId,
                          expectedBindingHash: candidate.expectedBindingHash,
                          expectedCandidateHash: proposal.bindingHash,
                        }),
                      "Binding updated.",
                    )
                  }
                >
                  Confirm rebind
                </Button>
              )}
              <Button size="sm" variant="ghost" onClick={() => setCandidate(null)}>
                Cancel rebind
              </Button>
            </section>
          )}
          {message && <p role="status">{message}</p>}
          <Button size="sm" variant="ghost" disabled={busy} onClick={() => void refresh()}>
            Refresh
          </Button>
        </div>
      )}
    </details>
  );
}
