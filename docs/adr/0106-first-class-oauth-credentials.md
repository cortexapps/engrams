# ADR 0106 — first-class OAuth credentials for pluggable harnesses

Status: Accepted (2026-07-31)

## Context

Harness descriptors currently model every human credential as one environment
variable. That works for Claude's setup token, but not for Codex. The Codex
descriptor asks users for `CODEX_ACCESS_TOKEN` while the harness only starts an
API-key login when `CODEX_API_KEY` exists. More importantly,
`CODEX_ACCESS_TOKEN` is OpenAI's Enterprise automation credential, not the
personal ChatGPT sign-in mechanism.

OpenAI documents managed ChatGPT device authorization as the headless Codex
flow. It works for personal ChatGPT accounts and managed workspaces. Enterprise
users retain their selected workspace's RBAC, retention, and residency policy;
an Enterprise administrator must enable device-code login. Programmatic
sessions continue to use API keys.

OAuth is not Codex-specific. Connectors and MCP servers will need the same
storage, subject isolation, lifecycle, and delivery primitives later.

## Decision

`HarnessAuth` gains one optional `user_oauth` declaration containing a stable
provider id and an `opaque_bundle` delivery mode. A descriptor may declare
`user_env` or `user_oauth`, never both. Codex declares provider `openai-codex`;
its programmatic `CODEX_API_KEY` contract is unchanged.

The coordinator owns a generic OAuth service:

- connections are keyed by opaque `(subject_kind, subject_id, provider)` and
  contain a KEK-sealed provider bundle, non-secret account metadata, a
  monotonic version, timestamps, and revocation state;
- short-lived flow rows contain only provider, subject, owner lease, expiry,
  and terminal status. Device codes, JSON-RPC messages, and tokens are never
  persisted;
- trusted provider drivers acquire flows, validate credential bundles, extract
  public metadata, and validate refreshed bundles;
- session creation binds only provider and subject. OAuth bytes never enter
  harness environment maps, profiles, events, or logs;
- an authenticated, size-bounded credential control channel lets only the
  bound session fetch its bundle and compare-and-swap a validated refresh.

The first driver runs the same pinned Codex app-server as the harness. It uses
`account/login/start` with `chatgptDeviceCode`, waits for
`account/login/completed`, and accepts the cache only after `account/read`
reports managed `chatgpt` authentication. The experimental
`chatgptAuthTokens` API is explicitly unsupported.

The harness materializes the bounded cache in a private `CODEX_HOME` with mode
`0600`, initializes managed auth, synchronizes validated refreshes, and removes
the cache before starting a thread. Version conflicts fetch the winning cache
at the next safe restart boundary.

User routes and Settings → Credentials expose connect, poll, retry,
disconnect, and non-secret account/workspace metadata. Missing human
credentials block launch with “Connect OpenAI.” Service-account owners keep
using the org API key path.

## Implementation

The accepted implementation landed in three additive layers: the generic
credential store, provider-driver lifecycle, and session control exchange;
the authenticated orchestrator routes and Credentials UI; and finally the
Codex managed-auth consumer plus descriptor flip. The Codex binary used by the
coordinator and harness is pinned through the same staging helper, and its
schema gate requires the device-login, managed-account, plan, and completion
surfaces used here.

Unit and conformance coverage exercises subject isolation, sealing, flow
leases and cleanup, compare-and-swap refreshes, personal and Enterprise
account shapes, broker delivery, removal before thread start, task ownership,
and user-facing lifecycle states. A real personal-account login remains a
deployment acceptance smoke because it requires an interactive OpenAI account;
the same is true of the optional live Enterprise smoke.

## Consequences

Personal ChatGPT users no longer need an OpenAI Platform organization or an
Enterprise workspace. Enterprise compatibility uses the same managed ChatGPT
protocol and preserves workspace policy. Enterprise access tokens remain out
of scope.

The store, driver registry, subject model, status API, and credential delivery
contract are consumer-neutral. Connector and MCP OAuth are intentionally not
migrated now, but can add providers and bindings without introducing another
token store.

OAuth rollout is additive: store/service and control wire, then orchestrator
and UI, then the Codex descriptor flip. The final acceptance smoke uses a real
personal account. Fake app-server tests cover personal and Enterprise plan
metadata; a live Enterprise smoke is recorded when a workspace is available.

## References

- [OpenAI Codex authentication](https://developers.openai.com/codex/auth)
- [Codex app-server authentication](https://developers.openai.com/codex/app-server#auth-endpoints)
