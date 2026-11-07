import { createClient } from "@connectrpc/connect";
import { useQuery, useTransport } from "@connectrpc/connect-query";
import { useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  getMcpSetup,
  listConnections,
} from "@/gen/engram/app/v1/integration-IntegrationService_connectquery";
import {
  IntegrationService,
  type GetMcpSetupResponse,
  type IntegrationConnection,
} from "@/gen/engram/app/v1/integration_pb";
import { McpCatalogReview } from "./McpCatalogReview";
import {
  MCP_CALLBACK_PATH,
  saveMcpFlow,
  assertMcpFlowAvailable,
  clearMcpFlowForConnection,
  clearMcpFlow,
} from "@/mcp-callback-bootstrap";

export function McpConnections({ setup }: { setup: GetMcpSetupResponse }) {
  const transport = useTransport();
  const client = createClient(IntegrationService, transport);
  const connections = useQuery(listConnections, {});
  const [name, setName] = useState("");
  const [alias, setAlias] = useState("");
  const [destination, setDestination] = useState(0);
  const [kind, setKind] = useState("api_key");
  const [registration, setRegistration] = useState("");
  const [sharing, setSharing] = useState(false);
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const selected = setup.destinations[destination];
  const redirect = `${window.location.origin}${MCP_CALLBACK_PATH}`;
  const registrations = setup.registrations.filter(
    (item) =>
      item.redirectUri === redirect &&
      item.resource?.zoneId === selected?.zoneId &&
      item.resource?.endpoint === selected?.endpoint,
  );
  async function createConnection() {
    if (
      !selected ||
      !name.trim() ||
      !alias.trim() ||
      (kind === "oauth" && (!sharing || !registrations.some((r) => r.id === registration)))
    )
      return;
    setBusy(true);
    setMessage("");
    try {
      await client.createConnection({
        alias: alias.trim(),
        provider: "mcp",
        displayName: name.trim(),
        mcp: {
          endpoint: selected.endpoint,
          zone: selected.zoneId,
          authKind: kind,
          apiKeySecretRef: "",
          oauthClientRegistrationId: kind === "oauth" ? registration : "",
          shareWithOrgSessions: kind === "oauth" && sharing,
        },
      });
      setName("");
      setAlias("");
      await connections.refetch();
    } catch {
      setMessage("Could not create the connection. Check the settings and try again.");
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="flex flex-col gap-3">
      {(connections.data?.connections ?? [])
        .filter((c) => c.provider === "mcp")
        .map((connection) => (
          <McpConnection
            key={connection.id}
            connection={connection}
            setup={setup}
            refresh={() => connections.refetch()}
          />
        ))}
      {!setup.discoveryAvailable && (
        <p className="text-sm text-muted-foreground">Discovery is unavailable.</p>
      )}
      {setup.enabled && setup.destinations.length > 0 && (
        <>
          <h3 className="text-sm font-semibold">Add a connection</h3>
          <label className="text-sm">
            Connection name
            <Input
              aria-label="Connection name"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </label>
          <label className="text-sm">
            Connection alias
            <Input
              aria-label="Connection alias"
              value={alias}
              onChange={(e) => setAlias(e.target.value)}
            />
          </label>
          <label className="text-sm">
            Approved endpoint
            <select
              aria-label="Approved endpoint"
              value={destination}
              onChange={(e) => setDestination(Number(e.target.value))}
            >
              {setup.destinations.map((d, i) => (
                <option key={`${d.zoneId}:${d.endpoint}`} value={i}>
                  {d.zoneId} · {d.endpoint}
                </option>
              ))}
            </select>
          </label>
          <label className="text-sm">
            Authentication
            <select
              aria-label="Authentication"
              value={kind}
              onChange={(e) => setKind(e.target.value)}
            >
              <option value="api_key">API key</option>
              <option value="oauth">OAuth</option>
            </select>
          </label>
          {kind === "oauth" && (
            <>
              <label className="text-sm">
                Registered client
                <select
                  aria-label="Registered client"
                  value={registration}
                  onChange={(e) => setRegistration(e.target.value)}
                >
                  <option value="">Select a client</option>
                  {registrations.map((r) => (
                    <option key={r.id} value={r.id}>
                      {r.id}
                    </option>
                  ))}
                </select>
              </label>
              <label className="text-sm">
                <input
                  type="checkbox"
                  checked={sharing}
                  onChange={(e) => setSharing(e.target.checked)}
                />{" "}
                Share this account with organization sessions
              </label>
            </>
          )}
          <Button
            size="sm"
            className="self-start"
            onClick={createConnection}
            disabled={
              busy ||
              !selected ||
              !name.trim() ||
              !alias.trim() ||
              (kind === "oauth" && (!sharing || !registration))
            }
          >
            Create connection
          </Button>
          {message && <p role="alert">{message}</p>}
        </>
      )}
    </div>
  );
}

function McpConnection({
  connection,
  setup,
  refresh,
}: {
  connection: IntegrationConnection;
  setup: GetMcpSetupResponse;
  refresh: () => unknown;
}) {
  const client = createClient(IntegrationService, useTransport());
  const status = useQuery(getMcpSetup, { connectionId: connection.id });
  const secret = useRef<HTMLInputElement>(null);
  const [header, setHeader] = useState("authorization");
  const [editing, setEditing] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [editName, setEditName] = useState(connection.displayName);
  const [editAlias, setEditAlias] = useState(connection.alias);
  const [editDestination, setEditDestination] = useState("");
  const [editKind, setEditKind] = useState("api_key");
  const [editRegistration, setEditRegistration] = useState("");
  const [editSharing, setEditSharing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const current = status.data?.current?.connection ?? connection;
  async function refreshConnection() {
    await Promise.all([status.refetch(), refresh()]);
  }
  async function run(action: () => Promise<unknown>) {
    setBusy(true);
    setMessage("");
    try {
      await action();
      await refreshConnection();
    } catch {
      setMessage(
        "The action could not be completed. Reload the connection and try again or recover it.",
      );
    } finally {
      if (secret.current) secret.current.value = "";
      setBusy(false);
    }
  }
  const request = {
    connectionId: current.id,
    expectedRevision: current.configRevision,
    expectedGeneration: current.authGeneration,
  };
  async function authorize() {
    assertMcpFlowAvailable(sessionStorage);
    const reg = setup.registrations.find((r) => r.id === current.mcp?.oauthClientRegistrationId);
    const redirectUri = `${window.location.origin}${MCP_CALLBACK_PATH}`;
    if (!reg || reg.redirectUri !== redirectUri) throw new Error("Callback is not registered");
    const result = await client.authorizeMcp({ ...request, shareWithOrgSessions: true });
    saveMcpFlow(sessionStorage, {
      connectionId: current.id,
      flowId: result.flowId,
      configRevision: result.configRevision,
      authGeneration: result.authGeneration,
      redirectUri,
      expiresAt: result.expiresAt,
    });
    window.location.assign(result.authorizationUrl);
  }
  return (
    <section
      className="flex flex-col gap-2 rounded-md border p-3"
      aria-label={`${connection.displayName} MCP connection`}
    >
      <h3 className="text-sm font-semibold">{connection.displayName}</h3>
      <p className="text-xs text-muted-foreground">
        {current.mcp?.zone} · {current.mcp?.endpoint} · {current.enabled ? "Enabled" : "Disabled"}
      </p>
      <p className="text-xs">
        Credential: {status.data?.current?.credential?.status ?? "unavailable"}{" "}
        {status.data?.current?.credential?.reference ?? ""}
      </p>
      {!setup.enabled && (
        <p className="text-xs text-muted-foreground">New external tool dispatch is disabled.</p>
      )}
      {!setup.recoveryAvailable && (
        <p className="text-xs text-muted-foreground">Credential recovery is unavailable.</p>
      )}
      {!editing ? (
        <Button
          size="sm"
          variant="outline"
          disabled={!setup.enabled}
          onClick={() => {
            setEditName(current.displayName);
            setEditAlias(current.alias);
            setEditDestination(
              String(
                setup.destinations.findIndex(
                  (d) => d.zoneId === current.mcp?.zone && d.endpoint === current.mcp?.endpoint,
                ),
              ),
            );
            setEditKind(current.mcp?.authKind ?? "api_key");
            setEditRegistration(current.mcp?.oauthClientRegistrationId ?? "");
            setEditSharing(current.mcp?.shareWithOrgSessions ?? false);
            setEditing(true);
          }}
        >
          Edit connection
        </Button>
      ) : (
        <div className="flex flex-col gap-2">
          <p className="text-xs">
            Changing connection settings can require new authorization and catalog review.
          </p>
          <label>
            Name
            <Input
              aria-label={`Name for ${connection.displayName}`}
              value={editName}
              onChange={(e) => setEditName(e.target.value)}
            />
          </label>
          <label>
            Alias
            <Input
              aria-label={`Alias for ${connection.displayName}`}
              value={editAlias}
              onChange={(e) => setEditAlias(e.target.value)}
            />
          </label>
          <label>
            Approved endpoint
            <select
              aria-label={`Endpoint for ${connection.displayName}`}
              value={editDestination}
              onChange={(e) => {
                setEditDestination(e.target.value);
                setEditRegistration("");
              }}
            >
              <option value="-1">Select an approved endpoint</option>
              {setup.destinations.map((d, i) => (
                <option key={`${d.zoneId}:${d.endpoint}`} value={i}>
                  {d.zoneId} · {d.endpoint}
                </option>
              ))}
            </select>
          </label>
          <label>
            Authentication
            <select
              aria-label={`Authentication for ${connection.displayName}`}
              value={editKind}
              onChange={(e) => {
                setEditKind(e.target.value);
                setEditRegistration("");
              }}
            >
              <option value="api_key">API key</option>
              <option value="oauth">OAuth</option>
            </select>
          </label>
          {editKind === "oauth" && (
            <>
              <label>
                Registered client
                <select
                  aria-label={`Registered client for ${connection.displayName}`}
                  value={editRegistration}
                  onChange={(e) => setEditRegistration(e.target.value)}
                >
                  <option value="">Select a client</option>
                  {setup.registrations
                    .filter(
                      (r) =>
                        r.redirectUri === `${window.location.origin}${MCP_CALLBACK_PATH}` &&
                        r.resource?.zoneId ===
                          setup.destinations[Number(editDestination)]?.zoneId &&
                        r.resource?.endpoint ===
                          setup.destinations[Number(editDestination)]?.endpoint,
                    )
                    .map((r) => (
                      <option key={r.id} value={r.id}>
                        {r.id}
                      </option>
                    ))}
                </select>
              </label>
              <label>
                <input
                  type="checkbox"
                  checked={editSharing}
                  onChange={(e) => setEditSharing(e.target.checked)}
                />{" "}
                Share this account with organization sessions
              </label>
            </>
          )}
          <div className="flex gap-2">
            <Button
              size="sm"
              disabled={
                busy ||
                !setup.enabled ||
                !editName.trim() ||
                !editAlias.trim() ||
                !setup.destinations[Number(editDestination)] ||
                (editKind === "oauth" && (!editRegistration || !editSharing))
              }
              onClick={() =>
                void run(async () => {
                  const destination = setup.destinations[Number(editDestination)];
                  if (!destination) throw new Error("Missing endpoint");
                  await client.updateConnection({
                    id: current.id,
                    alias: editAlias.trim(),
                    displayName: editName.trim(),
                    expectedRevision: current.configRevision,
                    expectedGeneration: current.authGeneration,
                    mcp: {
                      endpoint: destination.endpoint,
                      zone: destination.zoneId,
                      authKind: editKind,
                      apiKeySecretRef: "",
                      oauthClientRegistrationId: editKind === "oauth" ? editRegistration : "",
                      shareWithOrgSessions: editKind === "oauth" && editSharing,
                    },
                  });
                  setEditing(false);
                })
              }
            >
              Save connection
            </Button>
            <Button size="sm" variant="outline" onClick={() => setEditing(false)}>
              Cancel
            </Button>
          </div>
        </div>
      )}
      {current.mcp?.authKind === "api_key" && (
        <>
          <label>
            Header
            <select
              aria-label={`Header for ${connection.displayName}`}
              value={header}
              onChange={(e) => setHeader(e.target.value)}
            >
              <option value="authorization">authorization</option>
              <option value="x-api-key">x-api-key</option>
              <option value="api-key">api-key</option>
            </select>
          </label>
          <label>
            API key
            <Input
              ref={secret}
              type="password"
              autoComplete="off"
              aria-label={`API key for ${connection.displayName}`}
            />
          </label>
          <Button
            size="sm"
            disabled={busy || !setup.enabled}
            onClick={() =>
              void run(async () => {
                const value = secret.current?.value ?? "";
                if (!value) throw new Error("Missing key");
                await client.putMcpCredential({ ...request, kind: "api_key", header, value });
              })
            }
          >
            Save key
          </Button>
        </>
      )}
      {current.mcp?.authKind === "oauth" && (
        <>
          {setup.registrations.find((item) => item.id === current.mcp?.oauthClientRegistrationId)
            ?.confidential && (
            <>
              <p className="text-xs">
                Client secret:{" "}
                {status.data?.clientSecretConfigured ? "configured" : "not configured"}
              </p>
              <label>
                Client secret
                <Input
                  ref={secret}
                  type="password"
                  autoComplete="off"
                  aria-label={`Client secret for ${connection.displayName}`}
                />
              </label>
              <Button
                size="sm"
                disabled={busy || !setup.enabled}
                onClick={() =>
                  void run(async () => {
                    const value = secret.current?.value ?? "";
                    if (!value) throw new Error("Missing secret");
                    await client.putMcpCredential({
                      ...request,
                      kind: "client_secret",
                      header: "authorization",
                      value,
                    });
                  })
                }
              >
                Save client secret
              </Button>
            </>
          )}
          <Button
            size="sm"
            disabled={
              busy ||
              !setup.enabled ||
              (!!setup.registrations.find(
                (item) => item.id === current.mcp?.oauthClientRegistrationId,
              )?.confidential &&
                !status.data?.clientSecretConfigured)
            }
            onClick={() => void run(authorize)}
          >
            Authorize
          </Button>
        </>
      )}
      <div className="flex flex-wrap gap-2">
        <Button
          size="sm"
          variant="outline"
          disabled={busy || !setup.recoveryAvailable}
          onClick={() =>
            void run(() =>
              client
                .recoverMcpCredential(request)
                .then(() => clearMcpFlowForConnection(sessionStorage, current.id)),
            )
          }
        >
          Recover
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={busy || !setup.recoveryAvailable}
          onClick={() => void run(() => client.disconnectMcp(request))}
        >
          Disconnect
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={busy || !setup.enabled || (!current.enabled && !setup.discoveryAvailable)}
          onClick={() =>
            void run(() =>
              client.setConnectionEnabled({
                id: current.id,
                expectedRevision: current.configRevision,
                enabled: !current.enabled,
              }),
            )
          }
        >
          {current.enabled ? "Disable" : "Enable"}
        </Button>
      </div>
      {confirmDelete ? (
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-sm">
            Delete this connection? Profiles that use it must be updated first.
          </span>
          <Button
            size="sm"
            variant="destructive"
            disabled={busy || !setup.enabled}
            onClick={() =>
              void run(() =>
                client.deleteConnection({
                  id: current.id,
                  expectedRevision: current.configRevision,
                  expectedGeneration: current.authGeneration,
                }),
              )
            }
          >
            Delete connection
          </Button>
          <Button size="sm" variant="outline" onClick={() => setConfirmDelete(false)}>
            Cancel
          </Button>
        </div>
      ) : (
        <Button
          size="sm"
          variant="outline"
          disabled={busy || !setup.enabled}
          onClick={() => setConfirmDelete(true)}
        >
          Remove connection
        </Button>
      )}
      <McpCatalogReview
        connectionId={current.id}
        revision={current.configRevision}
        authGeneration={current.authGeneration}
        discoveryAvailable={setup.discoveryAvailable}
        refreshConnection={refreshConnection}
      />
      <div className="text-xs text-muted-foreground">
        If this tab has lost authorization context, recover the connection and clear this tab before
        starting again.
      </div>
      <Button
        size="sm"
        variant="outline"
        disabled={busy}
        onClick={() => {
          try {
            clearMcpFlow(sessionStorage);
            setMessage(
              "This tab's authorization context was cleared. Recover any pending connection before starting again.",
            );
          } catch {
            setMessage("This tab could not clear authorization context.");
          }
        }}
      >
        Clear this tab's authorization context
      </Button>
      {message && <p role="alert">{message}</p>}
    </section>
  );
}
