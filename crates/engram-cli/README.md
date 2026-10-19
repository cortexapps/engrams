# engrams CLI

The product CLI. Talks **only to the orchestrator** (Connect RPC over
HTTP/1.1 on `/rpc`, plus the SSE events + better-auth HTTP routes) — the
coordinator is an internal service the CLI never dials. A Rust port of the
Bun CLI it replaces, with the same commands, flags, output and exit codes.

## Auth

Exactly the `gh` model:

1. `ENGRAMS_API_KEY` env var (CI/scripts) — sent as `x-api-key`, never stored.
2. `engrams auth login` — RFC 8628 device flow: copy the one-time code,
   approve in the browser (the dashboard's `/device` page), and the CLI
   exchanges the short-lived session for a durable **user-owned** API key
   (`ApiKeyService.CreateCliKey`) stored in `~/.config/engrams/hosts.json`
   (0600, keyed by host URL). Revoke any time via `engrams auth logout` or
   Settings → API keys.

The target host: `--url` > `ENGRAMS_URL` > `https://engrams.cortex.io`.
For local dev (`just dev`): `ENGRAMS_URL=http://localhost:8787`.

## Commands

```
engrams auth login|logout|status
engrams task create --profile <name|id> [--prompt …]   # THE product create
engrams task list|get|delete
engrams session create|list|get|exec|delete|logs|log|resume|prompt
engrams image list|enable|config|update|disable|refresh|jobs|job|poll-job|retry-job
engrams registry add|list|rm
engrams host list|get|drain|delete|uncordon|evacuate
engrams profile list|get
engrams apikey create|list|revoke                      # admin service keys
engrams admin flush|evict-idle|gc
engrams tui [session-id] [--no-alt-screen] [--no-mouse] [--ax] # terminal client (ADR 0124)
```

`engrams tui` opens the session list, or attaches to one session, in a
full-screen terminal UI. `--no-alt-screen` (or `ENGRAMS_TUI_NO_ALT_SCREEN=1`)
keeps the terminal's own scrollback instead: finished turns are written into
it and only the composer and footer stay live. `?` lists the keys.
`NO_COLOR` turns color off. The crate is `crates/engram-tui`.

Output contract: default = human tables; `--json` = pretty JSON on stdout
and nothing else; errors on stderr as `engrams: …` + exit 1. `session exec`
mirrors the remote exit code.

## Dev

```
cargo build -p engram-cli                 # -> target/debug/engrams
cargo nextest run -p engram-cli           # unit + stub-orchestrator tests
cargo build --profile cli -p engram-cli   # the shipped build (-> target/cli/engrams)
```

The protos come from `crates/engram-app-client` (prost-build over
`crates/engram-protocol/proto/engram/app/v1`).
