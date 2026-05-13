# Engram demo runbook — real Claude Code in Firecracker

Sister to `docs/demo-firecracker.md`. Same FC setup, but the
auto-spawned harness is the real `claude` CLI instead of the
predictable noop. Validates the Phase 5 wire (AgentMessage,
result_summary, two-tier idle eviction, `POST /sessions/:id/prompt`)
against an actual LLM-driven agent loop.

What this exercises:
- The Phase 5 chat-consumer wire shape: `AgentMessage` between
  `ToolCallStarted` / `ToolCallCompleted`, with consolidated text
  per assistant message and `result_summary` (≤ 4 KB) on each
  tool result. A Slack bot or web UI can render this directly
  off `GET /sessions/:id/events` SSE — no JSONL decoder.
- Back-and-forth via `engram session prompt <id> "..."`. Initial
  prompt at create; follow-ups land while the same Claude
  process keeps `--resume`-ing its own session id.
- Two-tier idle eviction: a session that's been Idle for 30s
  (soft) snapshot-evicts; the next prompt hot-resumes. A
  long-running tool call doesn't trip soft TTL; the 30-min hard
  TTL is the stuck-adapter backstop.
- Dead-session affordance: when the FC snapshot is invalidated,
  resume returns 410 Gone with `{"error":"snapshot_invalidated"}`
  and the operator forks the workspace to continue.

What this does **not** exercise:
- File-secret mounting. Claude bills against the operator's
  `ANTHROPIC_API_KEY` (API tier), not their Pro/Max subscription.
  Mounting `~/.claude/.credentials.json` is deferred.
- Per-user auth. The whole coord uses one `ANTHROPIC_API_KEY`.

## Prereqs (one-time)

Same as `docs/demo-firecracker.md` — provision the dev VM and
cache FC artifacts. Then on the host:

```
export ANTHROPIC_API_KEY=sk-ant-...
```

The coord's `EnvSecretStore` reads this; `apply_secrets_to_env`
plumbs it into the sandbox's env; the bootstrap inherits and the
in-VM `claude` process picks it up.

## Bake the image

```
/dev-vm run just fc-bake-claude
```

What this does:
1. Cross-compiles `engram-agentd`, `engram-bootstrap`, and the
   new `engram-harness-claude` for `x86_64-unknown-linux-musl`.
2. Builds a Docker image off `node:20-slim` with `git`,
   `ca-certificates`, and `@anthropic-ai/claude-code` installed.
3. Runs `engram image build` to produce an ext4 rootfs at
   `./var/engram/images/local/claude-demo/warm-1.ext4` with
   the three musl binaries injected at `/sbin/`.

Rootfs is ~400 MB. Acceptable for the demo.

## Run the coordinator

```
/dev-vm ssh "tmux new-session -d -s engram \
  'cd ~/engrams && /nix/.../nix develop --command \
   env ANTHROPIC_API_KEY=$ANTHROPIC_API_KEY \
       ENGRAM_KERNEL_IMAGE_PATH=$HOME/.cache/engram-fc-test/vmlinux-5.10.223 \
       ENGRAM_DEFAULT_IMAGE=warm-1 \
       ENGRAM_DEV_AUTO_AGENT=claude \
       just dev-firecracker > /tmp/engram-coord.log 2>&1'"
```

Notes:
- `ENGRAM_DEV_AUTO_AGENT=claude` flips `build_dev_agent` to
  pick `dev_claude_harness_path` (defaulted to
  `/sbin/engram-harness-claude` by the dev-firecracker recipe).
- Sessions take the chunked-OCI cold path; warm pools were retired
  with ADR 0008.

## Drive the demo

```
SID=$(engram session create --repo local://claude-demo \
        --prompt "list /workspace and tell me what's there")

# Watch the conversation live (the same SSE stream a Slack bot
# or web UI consumes — no Claude-specific decoders).
engram session log $SID --follow
# Expect:
#   RunStarted (run-... )
#   AgentMessage (assistant)  "I'll list /workspace..."
#   ToolCallStarted   bash {"command":"ls /workspace"}
#   ToolCallCompleted bash ok=true (24 ms)  result_summary="..."
#   AgentMessage (assistant)  "Here's what's in /workspace: ..."
#   RunCompleted ok=true
#   Idle

# Follow up. Claude --resume <session_id> kicks in transparently;
# the conversation continues on the same workspace.
engram session prompt $SID "now create a hello.txt with a haiku"
engram session log $SID --follow

# Long tool call (verifies new soft TTL doesn't fire mid-call).
engram session prompt $SID "run 'sleep 90 && echo done' as a bash tool call"
sleep 95
engram session log $SID
# Expect ToolCallCompleted ok=true with duration ~90 s. No
# mid-call eviction; soft TTL only fires after Idle.

# Idle eviction → hot auto-resume.
sleep 35
engram session get $SID                # status: idle (snapshot live)
engram session prompt $SID "what's in hello.txt?"
engram session log $SID --follow       # auto-resumed; new tool calls

# Fork from any event index. The new session sees the workspace
# as it was at event 5 but starts a fresh Claude — no prior
# conversation memory. Use the prompt to give context.
NEW=$(engram session fork $SID --at 5)
engram session prompt $NEW "given what you saw earlier, write tests"

# Dead-session affordance. Simulate snapshot invalidation:
sleep 35
rm -rf ./var/engram/snapshots/$SID
engram session prompt $SID "this should fail"
# Expect HTTP 410 Gone:
#   {"error":"snapshot_invalidated",
#    "hint":"fork to continue from the workspace"}
```

## Counterfactuals

- `ANTHROPIC_API_KEY` not set in the coord's env → `POST /sessions`
  fails at secret resolution with a clear "secret ANTHROPIC_API_KEY
  not available" error. VM doesn't spawn.
- `claude` not on PATH inside the rootfs (bake skipped step 2) →
  `RunCompleted{ok:false}` plus `Idle`. Session stays alive; caller
  retries.
- 5-minute pytest doesn't trip soft TTL → no mid-call eviction.
- Stuck adapter (claude hangs forever) → hard TTL fires after 30
  min → Dead status (since the adapter never emitted Idle).
- Tool-call cap (50 default) → adapter SIGTERMs claude, emits
  `RunCompleted{ok:false}` + `Idle`. Caller can prompt again.

## What this leaves for future rounds

- `AgentMessageChunk` for live-typing UIs (the v2 streaming cut).
- A real Slack bot / web app subscriber (out of scope here; the
  shape they need is what this round ships).
- Subscription billing via mounted `~/.claude/.credentials.json`
  (`SecretValue::File` + `BootstrapLaunch.files`). Half a day
  whenever it becomes the priority.
- Per-user auth via `SessionContext.user_id` → secret resolver.
- `engram-harness-opencode` and friends — the persistent-server
  adapter pattern is documented but not yet implemented.
- `HarnessCommand::Cancel` to interrupt a runaway run.
