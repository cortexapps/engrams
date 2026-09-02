---
title: API overview
description: The Connect RPC surface the dashboard and CLI use, and the few routes that live outside it.
sidebar:
  order: 2
---

The orchestrator serves one API, and the dashboard and the CLI are both clients of it. It is
[Connect RPC](https://connectrpc.com) over HTTP/1.1, so every method is a POST with a JSON or
protobuf body, callable with `curl` and with generated clients in any language Connect
supports.

```
POST /rpc/engram.app.v1.<Service>/<Method>
```

Authenticate with an API key in the `x-api-key` header. Personal keys come from
`engrams auth login`; service-account keys are minted by an admin in Settings or with
`engrams apikey create`.

```sh
curl -s "$ENGRAMS_URL/rpc/engram.app.v1.SessionService/ListSessions" \
  -H "x-api-key: $ENGRAMS_API_KEY" \
  -H "content-type: application/json" \
  -d '{}'
```

## Services

The orchestrator implements some services itself and forwards others to the coordinator.
The distinction does not matter to a client; every method is at the same path with the same
authentication.

| Service | What it covers |
|---|---|
| `TaskService` | Create, list, get, and delete tasks. |
| `SessionService` | Create, list, get, and delete sessions; exec; prompt; resume; snapshot; the conversation log. |
| `ProfileService` | List and get profiles. |
| `ImageService` | Enable, update, disable, and refresh images; registry credentials; enable jobs. |
| `HarnessCatalogService` | The harnesses a deployment offers, with their models, modes, and effort levels. |
| `FleetService` | Hosts, capacity, drain, cordon, and evacuation. Admin only. |
| `ApiKeyService` | Global API keys. Admin only. |
| `IntegrationService` | Connectors, connections, and the integration catalog. |
| `OrgSecretService` | Org secrets such as `ANTHROPIC_API_KEY`. Admin only. |
| `ModelRouterService` | Model routers such as OpenRouter and their model catalogs. |
| `ReviewService` | Pull request reviews and per-repository enrollment. |
| `AutomationService`, `AutomationRunService`, `WebhookRegistrationService` | Automations, their runs, and inbound webhooks. |
| `SpecService` | Specs, the collaborative documents written with an agent. |
| `ArtifactService` | Documents published from a session. |
| `MountCatalogService` | Skills available to profiles. |
| `OAuthCredentialService` | Per-user OAuth credentials for Claude Code, ChatGPT, Slack, Linear, and GitHub. |
| `PapercutService`, `PrRefService`, `IntegrationOpService`, `MintService` | Friction reports from agents, pull request references, integration operations, and short-lived credential minting. |

The service definitions are the `.proto` files under
[`crates/engram-protocol/proto/engram/app/v1/`](https://github.com/cortexapps/engrams/tree/main/crates/engram-protocol/proto/engram/app/v1)
in the repository. Generate a client with [Buf](https://buf.build) and the Connect plugin for
your language; the repository's `buf.gen.yaml` shows how the dashboard and CLI generate
theirs.

## Outside the RPC surface

A few things are plain HTTP routes because they stream or carry bytes.

| Route | What it does |
|---|---|
| `GET /api/v1/sessions/<id>/events` | The session's event stream as server-sent events. Send `Last-Event-ID` to resume after a disconnect. |
| file upload and download routes on a session | Move files in and out of a sandbox. `engrams session exec` and the dashboard's file panel use them. |
| a WebSocket route per session | The interactive shell relay behind the dashboard's shell tab. |
| `/api/auth/*` | Sign-in, the device-code flow that `engrams auth login` uses, and OAuth callbacks. |

## Versioning

The API is under `engram.app.v1` and is not yet frozen: methods are added and, less often,
changed between releases. A deployment's dashboard, CLI, and orchestrator are built from one
commit and move together; a client you write against the proto files should be regenerated
when you upgrade.
