# Session telemetry (OTel GenAI → Langfuse or any OTLP collector)

Engrams can export traces of agent sessions — turns, generations (model +
token usage + cost), and tool calls — to a customer-hosted
[Langfuse](https://langfuse.com) or to any OpenTelemetry collector. The
feature is off by default and costs nothing while unset.

## Architecture

```
guest (claude / codex / custom harness) ──native TLS──▶ model provider   (unchanged)
      │ HarnessEvent (shared wire enum, vsock)
      ▼
host-agent ──▶ coordinator ──▶ PG session_events (durable, replayable log)
                                     │
                orchestrator SessionListener → otel-exporter consumer(s)
                                     │  OTel GenAI spans, OTLP/HTTP JSON
                                     ▼
                one or more configured sinks (Langfuse, OTel collector, …)
```

Design properties:

- **Harness-agnostic.** The exporter consumes the normalized, persisted
  session-event log, not any agent's native output. Every harness gets
  session/turn/tool spans; a harness whose adapter emits the `generation`
  and `run_cost` events (claude and codex do) also gets model, token, and
  cost data.
- **Never gates the workload.** The exporter is a listener consumer strictly
  downstream of the persisted log, with its own durable cursor per sink. A
  slow or dead sink stalls only that sink's export position — never the
  session, the UI stream, another sink, or any other consumer.
- **At-least-once, idempotent.** Trace and span ids are deterministic
  (sha256 of task/session/run/tool ids + the recovery epoch). A replay
  after a restart re-posts byte-identical spans; Langfuse upserts them. A
  plain OTel collector pipeline without id-based dedup can show duplicate
  spans after an orchestrator restart mid-turn — this is the documented
  trade for never losing spans.
- **No pricing table.** Cost is passthrough only: the claude harness
  reports its own per-turn cost (`run_cost`, integer micro-USD); codex
  reports none. Backends that price from model + token counts (Langfuse)
  work either way.
- **Provider-native token semantics.** `gen_ai.usage.input_tokens` carries
  each provider's own accounting: Anthropic excludes cache reads from
  `input_tokens`; codex/OpenAI includes them. The cache token counts ride
  as separate attributes.

## Configuration

One env var on the orchestrator: `ENGRAM_TELEMETRY_SINKS`, a JSON array.
Unset or empty = telemetry off.

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

Fields per sink:

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Unique and STABLE. It keys the sink's durable per-session cursor (`consumer_cursors.consumer = "otel-exporter:<name>"`); renaming a sink restarts its export position. |
| `endpoint` | yes | Full OTLP/HTTP traces URL (http/https). Langfuse: `{LANGFUSE_HOST}/api/public/otel/v1/traces`. |
| `headers` | no | Extra request headers (auth). Values are secrets — the orchestrator never logs them. Source the whole env var from a k8s secret. |
| `captureContent` | no (default `false`) | When `true`, spans also carry prompt summaries, assistant message text, and tool arg/result summaries. Default is metadata only: model, tokens, cost, latency, tool names, ok/error. Decide this per sink — content leaving for a third-party-hosted backend is a distinct consent from token counts. |
| `serviceName` | no (default `"engrams"`) | OTel resource `service.name`. |

Langfuse auth: `Authorization: Basic <base64(public_key:secret_key)>` with
the project's API keys.

## Span model

| Span | Source | Key attributes |
|---|---|---|
| `agent_session` (trace root) | task lifetime; emitted at terminal | `session.id`, `user.id`, `engrams.task.id`, `engrams.harness`, `engrams.profile_id`, outcome; ERROR on `failed` |
| `agent_turn` | `run_started` → `run_completed`/`run_interrupted` | `gen_ai.operation.name=invoke_agent`, `engrams.prompt_id`, `engrams.cost_usd` (when the harness reports cost); ERROR on failure/interrupt |
| `chat <model>` | `generation` event (one per API call for claude; one per turn for codex) | `gen_ai.request.model`, `gen_ai.usage.input_tokens` / `output_tokens` / `cache_read_input_tokens` / `cache_creation_input_tokens`, `gen_ai.response.id` |
| `execute_tool <name>` | `tool_call_completed` (harness-measured `duration_ms` derives the start) | `gen_ai.tool.name`, `gen_ai.tool.call.id`; ERROR on `ok:false` |

The trace id derives from the ROOT task, so an ADR 0113 sub-session tree
shares one trace. Timestamps are coordinator-stamped (`at` in the event
payload); they include guest→host→coordinator transit, so treat sub-100ms
differences as noise.

## Local smoke against a real Langfuse

1. Run Langfuse locally (their compose bundles Postgres, ClickHouse, MinIO,
   Redis — pick host ports that do not collide with the engrams dev stack,
   which already uses 5435 for PG):

   ```sh
   git clone https://github.com/langfuse/langfuse.git /tmp/langfuse
   cd /tmp/langfuse && docker compose up -d
   # UI on http://localhost:3000 — create an org, a project, and API keys.
   ```

2. Point the dev orchestrator at it. In the worktree-local `.env` (Tilt
   reads shared-then-local; local wins):

   ```sh
   ENGRAM_TELEMETRY_SINKS='[{"name":"langfuse-local","endpoint":"http://localhost:3000/api/public/otel/v1/traces","headers":{"Authorization":"Basic <base64(pk:sk)>"},"captureContent":true}]'
   ```

3. `just dev`, then run one claude session and one codex session to
   completion (a short two-turn prompt each, with at least one tool call).

4. Verify in the Langfuse UI:
   - one trace per task, with `agent_session` → `agent_turn` → generations
     and tools nested;
   - generations show model + token columns for both harnesses, and cost
     for the claude turns (codex turns have no cost — expected);
   - re-running the orchestrator mid-session does not duplicate spans
     (same ids, upserted);
   - with `captureContent` off, no prompt/message/tool text appears in any
     span attribute.

## Troubleshooting

- **No traces at all**: confirm the orchestrator log line for a rejected
  `ENGRAM_TELEMETRY_SINKS` (a malformed value is a hard startup error), and
  that the session belongs to a task (`task_session` row) — sessions with
  no task export nothing.
- **A sink stopped advancing**: the exporter retries with backoff up to 8
  attempts per event, then drops that flush and advances (a warn log per
  drop, `component=otel-exporter`). Check the sink's availability; the
  cursor row is `consumer_cursors` with `consumer = 'otel-exporter:<name>'`.
- **Cost missing**: only the claude harness reports cost today, and only on
  harness bundles baked after the `run_cost` wire event landed.
