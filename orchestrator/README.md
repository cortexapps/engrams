# Orchestrator

The orchestrator uses Bun. Run `bun run typecheck` and the focused `bun test` targets for a change. Database tests require an isolated test database.

## External MCP startup

External MCP is disabled by default. `MCP_ENABLED=true` permits new managed operations. The only accepted flag values are `true` and `false`. Enabling it also requires `MCP_RUNTIME_CONFIG`, an operator-owned JSON file path. A worker zone must separately have `enabled: true`; its default is false.

The configuration has this structure:

```json
{
  "version": 1,
  "infrastructure": {
    "deploymentIdentity": "sha256:<64 lowercase hexadecimal characters>",
    "networkIdentity": "sha256:<64 lowercase hexadecimal characters>",
    "trustIdentity": "sha256:<64 lowercase hexadecimal characters>"
  },
  "caFiles": { "control": "/etc/engrams/mcp/control-ca.pem" },
  "control": {
    "url": "https://control.example.test/",
    "caName": "control",
    "certFile": "/etc/engrams/mcp/client.pem",
    "keyFile": "/etc/engrams/mcp/client-key.pem",
    "serverFingerprint": "<SHA-256 certificate fingerprint: 32 uppercase hexadecimal byte pairs separated by colons>"
  },
  "workers": [
    {
      "zoneId": "public",
      "deploymentId": "public-workers",
      "enabled": false,
      "tls": {
        "url": "https://worker.example.test/",
        "caName": "control",
        "certFile": "/etc/engrams/mcp/client.pem",
        "keyFile": "/etc/engrams/mcp/client-key.pem",
        "serverFingerprint": "<SHA-256 certificate fingerprint>"
      }
    }
  ],
  "registrations": [],
  "destinations": [{ "zoneId": "public", "endpoint": "https://tools.example.test/custom/rpc" }]
}
```

Replace the example identities, paths, addresses and fingerprints with operator values. The loader rejects unknown fields and duplicate zones. It reads certificate and key files into memory and hashes the actual CA file contents. The live coordinator publication must match the configured infrastructure and CA identities. A configured address alone does not authorize a destination. Each worker deployment ID is a stable admission scope shared by all orchestrator processes that use that worker deployment.

Optional OAuth registrations have `id`, `clientId`, `confidential`, `redirectUri`, `issuer`, `resource` (`zoneId` and `endpoint`), and `scopes`. Resource credentials and OAuth client secrets are stored through the credential control service. They do not belong in this file.

Optional `destinations` lists up to 128 exact MCP resource endpoints, including servers that use API keys without OAuth registrations. Each entry has `zoneId` and a canonical HTTPS `endpoint`; paths are preserved. The setup UI combines these entries with OAuth registration resources, removes duplicates, and checks the live publication and both enable switches on each read. This list supplies choices, not authority. Unpublished endpoints are omitted. Authorization-server and token URLs are not MCP resource choices.

Keep the configuration path set when disabling new dispatch to retain authenticated lifecycle reads and terminal reconciliation. Existing journal expiry, cancellation bookkeeping, and committed completion recovery stay installed even without a configuration file. Expiry scans persisted scopes, including removed worker zones. Disabling dispatch does not replay external effects. A held or uncertain managed creation retains its durable identity; the orchestrator does not reconstruct lost secret-bearing creation bytes.

The application installs the shared runtime before starting workflows. Recovery and managed-session scanners stop before the listener manager and workflow engine during shutdown. A new managed binding becomes dispatchable only after the coordinator reports the exact finalized boot as started and the current host policy acknowledgement matches it.
