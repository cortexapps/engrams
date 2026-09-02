---
title: Session telemetry
description: Export every session's turns, model calls, and tool calls as OpenTelemetry spans to Langfuse or any collector.
sidebar:
  order: 5
---

engrams can export traces of agent sessions, with a span for each turn, each model call with
its tokens and cost, and each tool call, to a Langfuse you host or to any OpenTelemetry
collector. The feature is off by default and costs nothing while it is off.

```
guest (claude, codex, or a custom harness) ──native TLS──▶ model provider   (unchanged)
      │ harness events over vsock
      ▼
host agent ──▶ coordinator ──▶ Postgres session_events (the durable, replayable log)
                                     │
                orchestrator listener → exporter, one per sink
                                     │  OpenTelemetry GenAI spans, OTLP/HTTP JSON
                                     ▼
                Langfuse, an OpenTelemetry collector, or both
```

Four properties follow from that shape.

**It is harness-agnostic.** The exporter reads the normalized session-event log, not any
agent's native output. Every harness gets session, turn, and tool spans; a harness whose
adapter reports generations and cost, which Claude Code and Codex do, also gets model, token,
and cost data.

**It never gates the workload.** The exporter is a listener strictly downstream of the
persisted log, with its own durable cursor per sink. A slow or dead sink stalls only that
sink's export position, never the session, the dashboard's stream, or another sink.

**It is at-least-once, with stable ids.** Trace and span ids are derived from the task,
session, run, and tool ids, so a replay after a restart re-posts byte-identical spans and
Langfuse upserts them. A plain collector pipeline without id-based dedup can show duplicate
spans after an orchestrator restart mid-turn; that is the documented trade for never losing
one.

**There is no pricing table.** Cost is passthrough: the Claude Code harness reports its own
per-turn cost in micro-dollars, and Codex reports none. Backends that price from model and
token counts, like Langfuse, work either way. Token counts carry each provider's own
accounting; Anthropic excludes cache reads from `input_tokens` and OpenAI includes them, and
the cache counts ride as separate attributes.

## Configuration

One environment variable on the orchestrator, `ENGRAM_TELEMETRY_SINKS`, holds a JSON array of
sinks. Unset or empty means off.

```json
[
  {
    "name": "langfuse",
    "endpoint": "https://langfuse.example.com/api/public/otel/v1/traces",
    "headers": { "Authorization": "Basic <base64(pk-lf-…:sk-lf-…)>" },
    "captureContent": false,
    "serviceName": "engrams"
  },
  {
    "name": "collector",
    "endpoint": "http://otel-collector:4318/v1/traces"
  }
]
```

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Unique and stable. It keys the sink's durable per-session cursor, so renaming a sink restarts its export position from the beginning. |
| `endpoint` | yes | The full OTLP/HTTP traces URL. For Langfuse: `<host>/api/public/otel/v1/traces`. |
| `headers` | no | Extra request headers, which is where auth goes. Values are secrets and are never logged; source the whole variable from a Kubernetes Secret. |
| `captureContent` | no, default `false` | When `true`, spans also carry prompt summaries, assistant text, and tool argument and result summaries. The default is metadata only: model, tokens, cost, latency, tool names, ok or error. Decide this per sink; content leaving for a third party is a different consent from token counts. |
| `serviceName` | no, default `engrams` | The OpenTelemetry resource `service.name`. |

Langfuse authenticates with `Authorization: Basic <base64(public_key:secret_key)>` using a
project's API keys.

## The span model

| Span | Emitted from | Key attributes |
|---|---|---|
| `agent_session` (the trace root) | the task's lifetime, at its end | `session.id`, `user.id`, `engrams.task.id`, `engrams.harness`, `engrams.profile_id`, the outcome; ERROR when the task failed |
| `agent_turn` | a run's start to its completion or interruption | `gen_ai.operation.name=invoke_agent`, `engrams.prompt_id`, `engrams.cost_usd` when the harness reports cost; ERROR on failure or interrupt |
| `chat <model>` | a generation event, one per API call for Claude Code and one per turn for Codex | `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`, `gen_ai.response.id` |
| `execute_tool <name>` | a tool call's completion, with its start derived from the harness's own duration | `gen_ai.tool.name`, `gen_ai.tool.call.id`; ERROR when the tool reported an error |

The trace id derives from the root task, so a tree of sub-sessions shares one trace.
Timestamps are stamped by the coordinator and include the transit from the VM, so treat
differences under 100 ms as noise.

## Try it against a local Langfuse

Run Langfuse locally. Its compose file bundles Postgres, ClickHouse, MinIO, and Redis, so pick
host ports that do not collide with the dev stack, which uses 5435 for Postgres:

```sh
git clone https://github.com/langfuse/langfuse.git /tmp/langfuse
cd /tmp/langfuse && docker compose up -d
# The UI is on http://localhost:3000. Create an org, a project, and API keys.
```

Point the dev orchestrator at it from `.env`:

```sh
ENGRAM_TELEMETRY_SINKS='[{"name":"langfuse-local","endpoint":"http://localhost:3000/api/public/otel/v1/traces","headers":{"Authorization":"Basic <base64(pk:sk)>"},"captureContent":true}]'
```

Start the stack with `just dev` and run one session to completion with at least one tool
call. In Langfuse you should see one trace for the task with the session, turn, generation,
and tool spans nested; model and token columns on the generations; and, if you turn
`captureContent` off and run again, no prompt or message text anywhere in the attributes.

## Troubleshooting

**No traces at all.** A malformed `ENGRAM_TELEMETRY_SINKS` is a startup error, so check the
orchestrator's log for it. Sessions that do not belong to a task export nothing.

**A sink stopped advancing.** The exporter retries each event with backoff up to eight times,
then drops that flush and moves on, with a warning in the log. Check the sink's availability.

**Cost is missing.** Only the Claude Code harness reports cost.
