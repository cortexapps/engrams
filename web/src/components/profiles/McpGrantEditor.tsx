import { useQuery } from "@connectrpc/connect-query";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { getMcpCatalogs } from "@/gen/engram/app/v1/integration-IntegrationService_connectquery";
import type { IntegrationConnection } from "@/gen/engram/app/v1/integration_pb";

export type McpProfileGrant = {
  connectionId: string;
  operation: string;
  resourceConstraints: string[];
  credentialScope?: string;
  mcp?: {
    upstreamName: string;
    automationWrite: boolean;
    unrestricted: boolean;
    argumentConstraintDocumentsJson: string[];
  };
};

export function McpGrantEditor({
  connection,
  grants,
  onChange,
  onAvailability,
}: {
  connection: IntegrationConnection;
  grants: McpProfileGrant[];
  onChange: (grants: McpProfileGrant[]) => void;
  onAvailability: (id: string, available: boolean) => void;
}) {
  const catalogs = useQuery(getMcpCatalogs, { connectionId: connection.id });
  const [toolName, setToolName] = useState("");
  const [document, setDocument] = useState("");
  const [unrestricted, setUnrestricted] = useState(false);
  const [automation, setAutomation] = useState(false);
  const own = grants.filter((g) => g.connectionId === connection.id && g.mcp);
  const publication = catalogs.data?.published;
  const currentCatalog =
    publication &&
    publication.revision === connection.catalogRevision &&
    publication.configRevision === connection.configRevision &&
    publication.authGeneration === connection.authGeneration;
  const canReview =
    !catalogs.isError &&
    !catalogs.isFetching &&
    !!currentCatalog &&
    own.every((grant) =>
      publication?.tools.some(
        (tool) => tool.enabled && tool.upstreamName === grant.mcp?.upstreamName,
      ),
    );
  const availabilityKey = `${connection.id}:${connection.configRevision}:${connection.authGeneration}:${connection.catalogRevision}`;
  useEffect(() => {
    onAvailability(availabilityKey, canReview);
  }, [availabilityKey, canReview, onAvailability]);
  const available = (
    currentCatalog && !catalogs.isError && !catalogs.isFetching ? publication.tools : []
  ).filter((t) => t.enabled && !own.some((g) => g.mcp?.upstreamName === t.upstreamName));
  function validDocument(value: string) {
    try {
      const parsed: unknown = JSON.parse(value);
      return (
        !!parsed &&
        typeof parsed === "object" &&
        !Array.isArray(parsed) &&
        Object.keys(parsed).length > 0
      );
    } catch {
      return false;
    }
  }
  function add() {
    if (
      !available.some((tool) => tool.upstreamName === toolName) ||
      (!unrestricted && !validDocument(document))
    )
      return;
    onChange([
      ...grants,
      {
        connectionId: connection.id,
        operation: "invoke",
        resourceConstraints: [],
        credentialScope: "org",
        mcp: {
          upstreamName: toolName,
          automationWrite: automation,
          unrestricted,
          argumentConstraintDocumentsJson: unrestricted ? [] : [document],
        },
      },
    ]);
    setToolName("");
    setDocument("");
    setUnrestricted(false);
    setAutomation(false);
  }
  return (
    <section
      className="flex flex-col gap-2 rounded-md border p-3"
      aria-label={`${connection.displayName} MCP grants`}
    >
      <h3 className="text-sm font-semibold">{connection.displayName} MCP grants</h3>
      {(catalogs.isError || (catalogs.data && !currentCatalog)) && (
        <p role="alert">The current reviewed catalog is unavailable.</p>
      )}
      {own.map((grant) => {
        const mcp = grant.mcp!;
        function update(next: typeof mcp) {
          onChange(grants.map((g) => (g === grant ? { ...g, mcp: next } : g)));
        }
        return (
          <div
            key={`${grant.operation}:${mcp.upstreamName}`}
            className="flex flex-col gap-2 rounded-md border p-2"
          >
            <strong>{mcp.upstreamName}</strong>
            <span className="text-xs">
              {grant.operation} · {grant.credentialScope || "org"}
            </span>
            {mcp.argumentConstraintDocumentsJson.map((value, index) => (
              <label key={index}>
                Constraint document {index + 1}
                <textarea
                  aria-label={`Constraint ${index + 1} for ${mcp.upstreamName}`}
                  value={value}
                  onChange={(e) =>
                    update({
                      ...mcp,
                      argumentConstraintDocumentsJson: mcp.argumentConstraintDocumentsJson.map(
                        (v, i) => (i === index ? e.target.value : v),
                      ),
                    })
                  }
                />
              </label>
            ))}
            <Button
              size="sm"
              variant="outline"
              onClick={() =>
                update({
                  ...mcp,
                  unrestricted: false,
                  argumentConstraintDocumentsJson: [...mcp.argumentConstraintDocumentsJson, ""],
                })
              }
            >
              Add constraint document
            </Button>
            {mcp.argumentConstraintDocumentsJson.length > 1 && (
              <Button
                size="sm"
                variant="outline"
                onClick={() =>
                  update({
                    ...mcp,
                    argumentConstraintDocumentsJson: mcp.argumentConstraintDocumentsJson.slice(
                      0,
                      -1,
                    ),
                  })
                }
              >
                Remove last constraint document
              </Button>
            )}
            <label>
              <input
                type="checkbox"
                checked={mcp.automationWrite}
                onChange={(e) => update({ ...mcp, automationWrite: e.target.checked })}
              />{" "}
              Allow automation writes
            </label>
            <label>
              <input
                type="checkbox"
                checked={mcp.unrestricted}
                onChange={(e) =>
                  update({
                    ...mcp,
                    unrestricted: e.target.checked,
                    argumentConstraintDocumentsJson: e.target.checked ? [] : [""],
                  })
                }
              />{" "}
              Allow unrestricted arguments
            </label>
            <p className="text-xs">
              {mcp.unrestricted
                ? "Unrestricted arguments explicitly selected."
                : `${mcp.argumentConstraintDocumentsJson.length} independent constraint documents.`}
            </p>
            <Button
              size="sm"
              variant="outline"
              onClick={() => onChange(grants.filter((g) => g !== grant))}
            >
              Remove {mcp.upstreamName}
            </Button>
          </div>
        );
      })}
      {available.length > 0 && (
        <div className="flex flex-col gap-2">
          <label>
            Reviewed tool
            <select
              aria-label={`Reviewed tool for ${connection.displayName}`}
              value={toolName}
              onChange={(e) => {
                setToolName(e.target.value);
                setAutomation(false);
              }}
            >
              <option value="">Choose a tool</option>
              {available.map((tool) => (
                <option key={tool.upstreamName} value={tool.upstreamName}>
                  {tool.upstreamName} · {tool.classification}
                </option>
              ))}
            </select>
          </label>
          <label>
            <input
              type="checkbox"
              checked={unrestricted}
              onChange={(e) => setUnrestricted(e.target.checked)}
            />{" "}
            Allow unrestricted arguments
          </label>
          {!unrestricted && (
            <label>
              Argument constraint JSON
              <textarea
                aria-label="Argument constraint JSON"
                value={document}
                onChange={(e) => setDocument(e.target.value)}
              />
            </label>
          )}
          <label>
            <input
              type="checkbox"
              checked={automation}
              disabled={
                available.find((tool) => tool.upstreamName === toolName)?.classification !== "write"
              }
              onChange={(e) => setAutomation(e.target.checked)}
            />{" "}
            Allow automation writes
          </label>
          <Button
            size="sm"
            variant="outline"
            disabled={!toolName || (!unrestricted && !validDocument(document))}
            onClick={add}
          >
            Add MCP grant
          </Button>
        </div>
      )}
    </section>
  );
}
