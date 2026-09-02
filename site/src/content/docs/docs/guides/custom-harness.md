---
title: Bring your own harness
description: Run an agent runtime that is not Claude Code or Codex inside engrams sessions.
sidebar:
  order: 7
---

engrams does not know what agent is running inside a session. It knows a harness: a
read-only bundle that the host mounts into the VM at boot and starts through a small,
fixed contract. Claude Code and Codex ship as harnesses. Yours can too, and it appears in
the same picker, with its own models, modes, and effort levels, and with the same
credential handling.

A harness is three things: a descriptor, an entry binary, and whatever that binary needs
next to it. This page is the contract for all three. It assumes you can write a small Rust
program.

## The bundle

A harness bundle is a directory tree. At its root:

| Path | Required | What it is |
|---|---|---|
| `harness.toml` | yes | The descriptor: name, models, modes, credentials, egress. |
| `harness` | yes | The entry binary, or whatever `exec` in the descriptor names. It must be executable. |
| anything else | no | Sidecars: your agent's own binary, its runtime, its assets. The entry binary finds them relative to its own path. |

Inside the VM the tree is mounted read-only at `/opt/engram/dyn/0/`, so the entry binary
runs as `/opt/engram/dyn/0/harness`. Build the entry binary as a static musl executable for
the guest architecture, because the image is anything with `/bin/sh` and may be glibc or
musl. If your agent is a glibc binary, ship a musl build beside it and pick at startup;
the Claude Code harness does exactly this for Alpine images.

You publish the tree as an OCI artifact. The artifact carries one layer of media type
`application/vnd.engram.harness.tar.v1+gzip`, a gzipped tar of the tree, and a config of
type `application/vnd.engram.harness.v1+json` whose body is `{"kind":"engram-harness-v1"}`.
Push it to any registry the coordinator can pull from, then register it under Settings →
Harnesses with its name and OCI reference. engrams pulls the artifact, validates the
descriptor, checks that the entry binary exists, packs the tree into a squashfs, stores it
in the blob store, and lists it in the catalog. Hosts fetch the squashfs on demand.

Names are 1 to 64 characters of lowercase letters, digits, hyphens, and underscores, and
may not collide with `claude` or `codex`. Deleting a harness removes it from the catalog;
sessions already running it finish.

## The descriptor

```toml
name  = "goose"
label = "Goose"
description = "Block's Goose agent. Needs a GOOSE_API_KEY org secret for programmatic runs."
exec = "harness"
router_protocols = ["openai_responses"]

[egress]
allow_hosts = ["telemetry.example.com"]

[native_egress]
allow_hosts = ["api.openai.com"]

[auth]
org_env = "GOOSE_API_KEY"
org_env_hint = "An admin sets an org secret named GOOSE_API_KEY."
user_env = "GOOSE_USER_TOKEN"
user_env_hint = "Paste the token from `goose auth token`."

[[models]]
id = "gpt-5.5"
label = "GPT-5.5"
default = true
env = { GOOSE_MODEL = "gpt-5.5" }

[[modes]]
id = "default"
label = "Build"
default = true

[[modes]]
id = "plan"
label = "Plan"

[[effort]]
id = "medium"
label = "Medium"
default = true
env = { GOOSE_REASONING = "medium" }
```

Every table rejects unknown keys, so a typo is a parse error, not a silent default.

| Field | Meaning |
|---|---|
| `name` | The stable id. Must match the name you register and the name a session selects. |
| `label`, `description` | Shown in the picker and on the Harnesses page. |
| `exec` | The entry binary, relative to the tree root. Default `harness`. No leading slash, no `..`. |
| `args` | Extra arguments appended after the ones engrams passes. |
| `router_protocols` | Which model-router protocols the harness can consume: `anthropic_messages`, `openai_responses`, both, or neither. |
| `[egress]` | Hosts the harness must reach on every launch, such as telemetry. |
| `[native_egress]` | The model provider's hosts, granted only on a direct launch. A routed launch gets the router's hosts instead. |
| `[auth] org_env` | The environment variable that carries the organization's credential for programmatic runs. engrams resolves it from an org secret of the same name. |
| `[auth] user_env` | The variable that carries a person's own credential for sessions they start. Stored per user under Settings → Credentials. |
| `[auth.user_oauth]` | Instead of `user_env`: a managed OAuth connection, as Codex uses for ChatGPT. The two are mutually exclusive. |
| `[[models]]`, `[[effort]]` | Options with an `id`, a `label`, at most one `default`, and an `env` map applied to the harness's environment. |
| `[[modes]]` | Options with no `env`. The selected mode arrives with each prompt, and the harness decides what it means. |

An option's `env` may not set the user credential variable, may not set the org credential
variable to anything but the empty string, and may not reference secrets. Model and effort
are pure environment: the descriptor is the whole mechanism, and the Claude Code harness
has no code for either.

## The launch

The in-guest daemon starts the entry binary with two flags and, in development, one
alternative:

```
/opt/engram/dyn/0/harness --port 1026 --session-id <uuid>     # Firecracker and Apple Virtualization
/opt/engram/dyn/0/harness --connect host:port --session-id <uuid>   # the process backend
```

`--vsock-host` is accepted as an alias of `--port`. Exactly one of the two dial flags is
present. The working directory is the image's `workdir`, or `/`. Standard input is closed
and standard output and error go to `/var/log/engram/harness.log`; never write to the
inherited console, because in a restored VM nothing drains it and the write blocks forever.

The environment holds the session's image env and profile secrets, the model and effort
env from the descriptor, the credential variable the descriptor named, `ENGRAM_SESSION_ID`,
`ENGRAM_SANDBOX_ID`, `ENGRAM_BINDING_EPOCH`, the egress proxy's CA in `SSL_CERT_FILE` and
its friends, `ENGRAM_TOOLS` with the JSON manifest of platform tools, and on a routed
launch `ENGRAM_MODEL_ROUTER_ID`, `_PROTOCOL`, `_BASE_URL`, and `_MODEL`. `ENGRAM_STATE_DIR`
names the directory that survives snapshots, `/workspace/.engrams` by default; anything the
harness must remember across an idle-evict and resume lives there, not in the process.

When a session resumes and the previous harness process is still alive, engrams does not
respawn it. It sends `SIGUSR1`, which means "drop your connection and dial again", because a
restore rebuilds the vsock device without closing the harness's socket. Handle it. If the
previous process exited, engrams starts a fresh one, and your state directory is how it
picks up where it left off.

## The wire

The harness dials the host, on vsock port 1026 in production or the TCP address in
development, and speaks length-prefixed frames: a four-byte big-endian length, then a
bincode-encoded message. The message types are Rust types in the `engram-harness-proto`
crate, and their variant order is pinned by golden tests, so the only safe evolution is a
trailing variant or field.

The first frame is `HarnessAttach` with the session id, sandbox id, binding epoch, and a
harness version string. The host answers `HarnessAttachAck`. A rejection is one of three:
`UnknownBinding` and `SessionMismatch` are transient, so back off and retry;
`Superseded` is fatal, because a newer harness generation owns the session.

After that, every frame is a `HarnessFrame` carrying either an event from the harness or a
command from the host.

**Events the harness sends.** `RunStarted` with the prompt id when a turn begins.
`AgentMessage` for each assistant message, with optional `AgentMessageChunk` frames before
it for streaming. `ToolCallStarted` and `ToolCallCompleted` around each tool call, with a
name, an id, and short summaries. `FileChanged` when a tool wrote a file, so the dashboard
can show a diff. `Generation` with the model and token counts after each model call, in
the provider's own accounting. `RunCost` once per turn if the agent reports cost.
`RunCompleted` or `RunInterrupted` when the turn ends. `TitleSuggested` if the agent names
the task. `ToolCallRequested` to ask the platform to run a tool from the manifest.

After every `RunCompleted` or `RunInterrupted` the harness sends exactly one of `Idle`,
`Parked`, or `Busy`. `Idle` means the session may be snapshotted and evicted. `Parked` means
the only outstanding work is a deferred tool call, such as a question waiting for a person,
and the session may be evicted meanwhile. `Busy` means background work is still running and
the idle timer must not start. Nothing else nominates a session for eviction, so a harness
that forgets this leaves VMs running forever.

**Commands the host sends.** `Prompt` with text, a prompt id, and an optional mode. Every
prompt, including the session's first, arrives this way; the harness acknowledges it by
sending `RunStarted` with the same id, and the platform retries until it does. If a turn is
in flight the harness queues the prompt and reports `PromptQueued`, and may later receive
`EditQueued` or `DequeueQueued`. `Interrupt` asks the harness to stop the current turn, by
whatever means the agent supports, and end it as `RunInterrupted`. `Shutdown` with a grace
period asks for a clean exit. `ToolResult` delivers the answer to a `ToolCallRequested`.
`Checkpoint` exists and every shipped harness ignores it.

Questions to a person and plan approval ride the tool path. The manifest in `ENGRAM_TOOLS`
lists `ask_user_question` and `exit_plan_mode` with their schemas; a harness that wires them
into its agent gets the dashboard's question forms and the Slack round-trip for free. A tool
marked `deferred` parks the turn until its result arrives.

## The SDK

The `engram-harness-sdk` crate owns the failure-prone half. It dials, attaches, retries with
backoff, handles `SIGUSR1`, re-sends unacknowledged events after a reconnect, and runs your
agent engine in a task that outlives any single connection, so a dropped host link never
aborts a turn. It also provides the prompt queue with per-prompt mode, the state directory
with named paths for the files a harness keeps, the mode stamp that survives resume, the
parked-call store for deferred tools, the question and plan-decision shapes, a stderr drain
so a chatty agent cannot deadlock on a full pipe, and a UTF-8-safe truncation for summaries.

The SDK and the crates it pulls in (`engram-harness-proto`, `engram-transport`, `engram-ids`) are
Apache-2.0. <!-- leak-ok: the harness SDK crates really are Apache-2.0 -->
Your harness links them without taking on the AGPL-3.0 terms that cover the rest of engrams.

The shape of a harness built on it:

```rust
let cfg = ConnectionConfig::from_args();
let ch = Channels::new();
let engine = tokio::spawn(run_agent(ch.command_rx, ch.event_tx.clone()));
let code = engram_harness_sdk::serve(cfg, engine, ch.command_tx, ch.event_rx, ch.reattach).await;
std::process::exit(code);
```

`run_agent` is yours: it receives commands, drives your agent's process or library, and
emits events. The Codex harness in the repository is the reference for an adapter that
wraps an external CLI, and the noop harness is the floor, a few hundred lines that speak the
protocol with no agent behind them.

A harness written in another language is not practical today. The wire has no JSON or
protobuf form and no schema file, so the working pattern is a thin Rust entry binary on the
SDK that runs your agent as a child process in any language you like.

## What a real adapter handles

The Claude Code harness is the honest measure. Beyond the wire it keeps one long-running
agent process per session rather than one per prompt, finds turn boundaries in the agent's
own output, stores the agent's conversation id in the state directory so a respawn resumes
the same conversation, maps the `plan` mode to the agent's read-only permission mode and
respawns cleanly when the mode changes, translates `Interrupt` into an in-band request and
then a signal, derives `Generation`, `RunCost`, `FileChanged`, and `TitleSuggested` from
the agent's stream, reports `Busy` while the agent's subagents work, and exposes the
platform tools to the agent through a local tool server it starts before the agent.

## Limits

Custom harnesses run on the Firecracker fleet. The macOS and process backends stage
bundles from a local directory and do not fetch a registered harness. Registration is in
the dashboard only; the CLI selects a harness with `--harness` but does not register one.
The router protocols are a closed set of two. The squashfs is packed root-owned without
extended attributes, so setuid bits and file ownership do not survive. And because the
wire is positional, a change to the protocol in a new engrams release requires a rebuild of
your harness against the new crate; the shipped harnesses are rebuilt in the same change.
