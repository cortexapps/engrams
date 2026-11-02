import { createClient } from "@connectrpc/connect";
import { useQuery, useTransport } from "@connectrpc/connect-query";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import { getMcpCatalogs } from "@/gen/engram/app/v1/integration-IntegrationService_connectquery";
import { IntegrationService, type McpTool } from "@/gen/engram/app/v1/integration_pb";

export function McpCatalogReview({
  connectionId,
  revision,
  authGeneration,
  discoveryAvailable,
}: {
  connectionId: string;
  revision: number;
  authGeneration: number;
  discoveryAvailable: boolean;
}) {
  const client = createClient(IntegrationService, useTransport());
  const catalogs = useQuery(getMcpCatalogs, { connectionId });
  const [review, setReview] = useState<McpTool[] | null>(null);
  const [draftId, setDraftId] = useState("");
  const [expectedRevision, setExpectedRevision] = useState(0);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  async function discover() {
    setBusy(true);
    setMessage("");
    setReview(null);
    try {
      await client.discoverMcp({ connectionId, expectedRevision: revision });
      await catalogs.refetch();
    } catch {
      setMessage("Discovery could not finish. Reload the connection and try again.");
    } finally {
      setBusy(false);
    }
  }
  function beginReview() {
    if (!catalogs.data?.draft) return;
    setDraftId(catalogs.data.draft.id);
    setExpectedRevision(catalogs.data.catalogRevision);
    setReview(
      catalogs.data.draft.tools.map((tool) => ({ ...tool, classification: "", enabled: false })),
    );
    setMessage("");
  }
  async function publish() {
    if (
      !review ||
      review.some((tool) => tool.classification !== "read" && tool.classification !== "write")
    )
      return;
    setBusy(true);
    setMessage("");
    try {
      await client.publishMcp({ connectionId, draftId, expectedRevision, tools: review });
      setReview(null);
      await catalogs.refetch();
      setMessage("Catalog published. Review the connection before enabling it.");
    } catch {
      setReview(null);
      await catalogs.refetch();
      setMessage(
        "Could not publish. The connection or catalog may have changed. Reload and review the draft again.",
      );
    } finally {
      setBusy(false);
    }
  }
  const published = catalogs.data?.published;
  const draft = catalogs.data?.draft;
  const draftNames = new Set(draft?.tools.map((tool) => tool.upstreamName));
  const staleDraft =
    !!draft &&
    (draft.configRevision !== catalogs.data?.configRevision ||
      draft.authGeneration !== authGeneration);
  const previous = new Map(published?.tools.map((tool) => [tool.upstreamName, tool]));
  return (
    <section className="flex flex-col gap-2 border-t pt-3" aria-label="MCP catalog">
      <h4 className="text-sm font-semibold">Tool catalog</h4>
      {catalogs.isError && <p role="alert">Catalog data is unavailable.</p>}
      {catalogs.data && (
        <p className="text-xs">
          Published revision {catalogs.data.catalogRevision} · {published?.tools.length ?? 0} tools
          · {draft ? "draft available" : "no draft"}
        </p>
      )}
      {published && (
        <details>
          <summary>Published tools</summary>
          {published.tools.map((tool) => (
            <p key={tool.upstreamName}>
              <code>{tool.upstreamName}</code> · {tool.classification} ·{" "}
              {tool.enabled ? "enabled" : "disabled"}
            </p>
          ))}
        </details>
      )}
      {published?.tools
        .filter((tool) => draft && !draftNames.has(tool.upstreamName))
        .map((tool) => (
          <p key={tool.upstreamName}>
            Removed from draft: <code>{tool.upstreamName}</code>
          </p>
        ))}
      {staleDraft && (
        <p role="alert">This draft is stale. Discover the tools again before publishing.</p>
      )}
      <div className="flex gap-2">
        <Button
          size="sm"
          variant="outline"
          disabled={busy || !discoveryAvailable}
          onClick={() => void discover()}
        >
          Discover tools
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={busy || catalogs.isError || catalogs.isFetching || !draft || staleDraft}
          onClick={beginReview}
        >
          Review draft
        </Button>
      </div>
      {review && (
        <div className="flex flex-col gap-3">
          {review.map((tool, index) => (
            <div key={tool.upstreamName} className="flex flex-col gap-2 rounded-md border p-3">
              <strong>{tool.upstreamName}</strong>
              <span className="text-xs">
                {!previous.has(tool.upstreamName)
                  ? "New"
                  : previous.get(tool.upstreamName)?.inputSchemaJson !== tool.inputSchemaJson ||
                      previous.get(tool.upstreamName)?.outputSchemaJson !== tool.outputSchemaJson ||
                      previous.get(tool.upstreamName)?.description !== tool.description
                    ? "Changed"
                    : "Unchanged"}
              </span>
              {previous.has(tool.upstreamName) && (
                <details>
                  <summary>Previous review</summary>
                  <p className="whitespace-pre-wrap">
                    {previous.get(tool.upstreamName)?.description}
                  </p>
                  <pre className="overflow-auto whitespace-pre-wrap">
                    {previous.get(tool.upstreamName)?.inputSchemaJson}
                  </pre>
                  <pre className="overflow-auto whitespace-pre-wrap">
                    {previous.get(tool.upstreamName)?.outputSchemaJson}
                  </pre>
                  <p>
                    {previous.get(tool.upstreamName)?.classification} ·{" "}
                    {previous.get(tool.upstreamName)?.enabled ? "enabled" : "disabled"}
                  </p>
                </details>
              )}
              <p className="whitespace-pre-wrap text-sm">{tool.description}</p>
              <details>
                <summary>Input schema</summary>
                <pre className="overflow-auto whitespace-pre-wrap">{tool.inputSchemaJson}</pre>
              </details>
              {tool.outputSchemaJson && (
                <details>
                  <summary>Output schema</summary>
                  <pre className="overflow-auto whitespace-pre-wrap">{tool.outputSchemaJson}</pre>
                </details>
              )}
              <label>
                Classification
                <select
                  aria-label={`Classification for ${tool.upstreamName}`}
                  value={tool.classification}
                  onChange={(event) =>
                    setReview(
                      review.map((item, i) =>
                        i === index ? { ...item, classification: event.target.value } : item,
                      ),
                    )
                  }
                >
                  <option value="">Choose read or write</option>
                  <option value="read">Read</option>
                  <option value="write">Write</option>
                </select>
              </label>
              <label>
                <input
                  type="checkbox"
                  checked={tool.enabled}
                  onChange={(event) =>
                    setReview(
                      review.map((item, i) =>
                        i === index ? { ...item, enabled: event.target.checked } : item,
                      ),
                    )
                  }
                />{" "}
                Enable this tool
              </label>
            </div>
          ))}
          <Button
            size="sm"
            disabled={busy || review.some((tool) => !tool.classification)}
            onClick={() => void publish()}
          >
            Publish reviewed catalog
          </Button>
        </div>
      )}
      {message && <p role="status">{message}</p>}
    </section>
  );
}
