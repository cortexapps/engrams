# Engram demo runbook — split-mode (`--mode=coordinator` + `--mode=host`)

Sister to `docs/demo-firecracker.md`. Same FC stack underneath, but
this one runs the coordinator and the host-agent as **two separate
processes on the same dev-vm** so the WS path between them is
exercised — the same code path a multi-host production deployment
runs against. Single-binary `just dev` / `--mode=all` doesn't
touch this path; bugs in the wire layer hide there.

What this exercises that `demo-firecracker.md` doesn't:

- `HostClient` over the wire (ADR 0011): every coord→host call is a
  bincode `Frame` over a real WebSocket — `CreateSandbox`,
  `StartAgent`, `BindHarnessSession`, `SendHarnessPrompt`,
  `GuestIp`, `Snapshot`, `Restore`, …
- Harness event forwarding (ADR 0011 §"NotifyKind::HarnessEvent"):
  the host-agent's `HarnessHub` ships each per-session event over
  the WS as a Notify; the coord's read loop re-emits via
  `state.emit` so SSE subscribers see the same stream they would
  in `--mode=all`.
- The filtering DNS proxy (ADR 0010): iptables REDIRECT for both
  `udp/53` and `tcp/53` lands at the host-agent's
  `engram-egress-proxy::dns` listener; `manifest.network.allow_hosts`
  decides allow vs `NXDOMAIN`.
- The TCP/443 SNI proxy: same as single-mode but reachable only
  because the host-agent's `SessionEgressPolicy` notify lands and
  registers the per-session state.
- Heartbeat liveness (`fix(coord): WS heartbeats refresh
  hosts.last_heartbeat_at`): without it, the dead-host detector
  reaps every external host ~90s after register.

## Prereqs

Same as `docs/demo-firecracker.md`:

```
/dev-vm bootstrap-local      # local: install mutagen + ssh config
/dev-vm start
/dev-vm bootstrap-remote     # remote: nix, firecracker, /dev/userfaultfd, etc.
/dev-vm sync-start
/dev-vm run bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh
```

## Launch scripts

`deploy/dev-split/run-coord.sh` and `deploy/dev-split/run-host.sh`
are reference launchers. Both source `.env` for
`ENGRAM_KEK_MASTER_KEY` and use the cached FC kernel.

```sh
# deploy/dev-split/run-coord.sh
set -a; source .env; set +a
exec /nix/var/nix/profiles/default/bin/nix develop --command bash -lc '
  export DATABASE_URL=postgres://engram:engram@localhost:5435/engram
  export ENGRAM_BIND_ADDR=127.0.0.1:8090
  export ENGRAM_MODE=coordinator
  export ENGRAM_AUTH_TOKENS=dev-split-token
  export ENGRAM_LOCAL_PATH=./var/engram
  export ENGRAM_BLOB_BACKEND=local
  export RUST_LOG=info,engram=debug
  exec ./target/debug/engram-coordinator
'
```

```sh
# deploy/dev-split/run-host.sh
set -a; source .env; set +a
KERNEL="${ENGRAM_KERNEL_IMAGE_PATH:-$HOME/.cache/engram-fc-test/vmlinux-5.10.223}"
exec sudo \
  ENGRAM_KEK_MASTER_KEY="$ENGRAM_KEK_MASTER_KEY" \
  ENGRAM_COORDINATOR_ENDPOINT=ws://127.0.0.1:8090 \
  ENGRAM_COORDINATOR_TOKEN=dev-split-token \
  ENGRAM_SANDBOX_BACKEND=firecracker \
  ENGRAM_SANDBOX_WORK_DIR=./var/sandboxes \
  ENGRAM_LOCAL_PATH=./var/engram \
  ENGRAM_BLOB_BACKEND=local \
  ENGRAM_KERNEL_IMAGE_PATH="$KERNEL" \
  ENGRAM_EGRESS_PROXY_PORT=9443 \
  ENGRAM_EGRESS_CA_SOURCE=local-disk \
  RUST_LOG=info,engram=debug \
  ./target/debug/engram-host-agent
```

Notes on the host script:
- `sudo` is required for `ip tuntap` / iptables. The binary runs
  fully linked (no nix-shell needed at runtime), so we don't need
  `-E` or to drag `$HOME` in.
- `ENGRAM_EGRESS_PROXY_PORT=9443` flips on the filtering proxy
  (TCP/443 MITM + DNS on udp/tcp/5353). Setting `0` disables it
  entirely and falls back to the legacy `ACCEPT VM→1.1.1.1:53`
  iptables shape; same as `demo-firecracker.md` defaults.
- Coord and host share the same `local_path` so a `local`
  `BlobStorage` lets one process see what the other wrote.

## Run

```sh
# 1. Postgres + registry one-shot (same as demo-firecracker).
/dev-vm run just db-up
/dev-vm run just registry-up
/dev-vm run just bootstrap

# 2. Build both binaries.
/dev-vm run bash -c "cargo build -p engram-coordinator -p engram-host-agent"

# 3. Bake the demo image to the registry. (NOT `just fc-bake-demo` —
#    that produces a local://demo image which the host-agent can't
#    pull from the registry. Use `just bake` instead.)
/dev-vm run just bake cortex/demo deploy/demo

# 4. Start coord + host in two tmux panes. tmux because background
#    SSH-spawned jobs die when the SSH session disconnects.
/dev-vm ssh "tmux new-session -d -s coord 'cd ~/engrams && exec bash deploy/dev-split/run-coord.sh > /tmp/coord.log 2>&1'"
sleep 6
/dev-vm ssh "tmux new-session -d -s host  'cd ~/engrams && exec bash deploy/dev-split/run-host.sh  > /tmp/host.log  2>&1'"
sleep 8
/dev-vm ssh 'tail -8 /tmp/coord.log; echo === host ===; tail -8 /tmp/host.log'

# 5. Confirm the host registered.
/dev-vm ssh '
curl -sf -H "authorization: Bearer dev-split-token" \
  http://127.0.0.1:8090/api/hosts | jq ".hosts[] | {id, status, last_heartbeat_at}"
'

# 6. Enable the demo image.
TAG=$(/dev-vm ssh 'curl -sf http://localhost:5001/v2/cortex/demo/tags/list | jq -r .tags[-1]')
/dev-vm ssh "curl -s -H 'authorization: Bearer dev-split-token' \
  -H 'content-type: application/json' \
  -X POST http://127.0.0.1:8090/api/enabled-images \
  -d '{\"image_uri\":\"localhost:5001/cortex/demo:$TAG\"}' | jq -r .image_uri"
```

## Drive a Claude harness session with a bogus key

```sh
SID=$(/dev-vm ssh "curl -s -H 'authorization: Bearer dev-split-token' \
  -H 'content-type: application/json' \
  -X POST http://127.0.0.1:8090/sessions \
  -d '{\"image\":\"localhost:5001/cortex/demo:$TAG\",
       \"harness\":{\"kind\":\"builtin\",\"name\":\"claude\"},
       \"secrets\":{\"ANTHROPIC_API_KEY\":\"sk-ant-bogus-99999999999999999999999999999999\"}}' \
  --max-time 240 | jq -r .session_id")

# Send a prompt; tail events.
/dev-vm ssh "curl -s -H 'authorization: Bearer dev-split-token' \
  -H 'content-type: application/json' \
  -X POST http://127.0.0.1:8090/sessions/$SID/prompt \
  -d '{\"text\":\"hello\"}' | jq -r .note"

/dev-vm ssh "curl -sN -H 'authorization: Bearer dev-split-token' \
  http://127.0.0.1:8090/sessions/$SID/events --max-time 120 | head -40"
```

Expected: within ~30 s the SSE stream surfaces an
`agent_message` with `role=assistant` and the text **"Invalid API
key · Fix external API key"**. That's the artifact proving the
full chain works:

1. Coord → host: `CreateSandbox` over WS.
2. Coord asks host for `GuestIp`; host returns `10.200.0.2`.
3. Coord → host: `NotifyKind::SessionEgressPolicy(...)`; host's
   PooledBackend registers the policy in the egress proxy.
4. Coord → host: `StartAgent`; host's FC backend opens a vsock
   to `engram-bootstrap`, pushes the launch frame; harness
   wrapper exec's Claude.
5. Coord → host: `SendHarnessPrompt`; host's hub forwards into
   the in-VM adapter; Claude starts.
6. Claude DNS lookup `api.anthropic.com` → iptables REDIRECT
   →  host-side DNS proxy → check QNAME against allow_hosts →
   forward to 1.1.1.1 → response (ADR 0010).
7. Claude TCP connect to the resolved IP:443 → iptables REDIRECT
   → host-side MITM proxy → SNI peek → bypass to upstream
   (ADR 0006).
8. Anthropic returns 401; Claude prints the auth error.
9. Harness wrapper translates the error to a
   `HarnessEvent::AgentMessage(assistant, …)`; host's hub fires
   the EventSink → `NotifyKind::HarnessEvent` over the WS.
10. Coord's `api/hosts.rs` read loop ingests → `state.emit` →
    Postgres `session_events` row → SSE bus → curl.

## Test a denied DNS lookup

The session above can also exercise the DNS filter end-to-end:

```sh
/dev-vm ssh "curl -s -H 'authorization: Bearer dev-split-token' \
  -H 'content-type: application/json' \
  -X POST http://127.0.0.1:8090/sessions/$SID/exec \
  -d '{\"command\":\"getent hosts api.anthropic.com; echo exit=\$?\"}' \
  --max-time 15 | jq -r .stdout"
# → IPv6 / IPv4 of api.anthropic.com, exit=0

/dev-vm ssh "curl -s -H 'authorization: Bearer dev-split-token' \
  -H 'content-type: application/json' \
  -X POST http://127.0.0.1:8090/sessions/$SID/exec \
  -d '{\"command\":\"getent hosts evil.exfil.com; echo exit=\$?\"}' \
  --max-time 15 | jq -r .stdout"
# → empty, exit=2  (NXDOMAIN — name not in allow_hosts)
```

The host log should show `DNS denied; responding NXDOMAIN ...
reason=NotInAllowList` for the second query. (`reason=UnknownGuest`
in those logs means the proxy didn't have a `SessionEgressPolicy`
registered for the source IP — that was the symptom of one of the
six wiring gaps `120b16e` fixed in split mode; it shouldn't appear
on a current build.)

## Stale-TAP cleanup between restarts

Each FC session leaves a `tap-engr-XXX` DOWN with a route for the
/30 it owned. The host-agent doesn't reap them on shutdown; the
kernel happily keeps multiple routes for `10.200.0.0/30` and can
pick a DOWN tap for new traffic, silently dropping packets. Between
restarts:

```sh
/dev-vm ssh '
for t in $(ip -br link show | awk "/^tap-engr-/{print \$1}"); do
  sudo ip link delete $t 2>/dev/null
done
sudo iptables -F; sudo iptables -t nat -F
sudo rm -rf ~/engrams/var/sandboxes/* 2>/dev/null
'
```

## What this doesn't yet exercise

- **Multiple hosts.** This runbook is one host process. Multi-host
  scheduling, snapshot-affinity routing, and cold-tier
  cross-host migration need a second VM.
- **Idle auto-eviction in split mode.** The driver still polls the
  coord's local hub, which doesn't see harness state in this
  topology. `known-issues.md#8` tracks the fix; sessions linger
  until hard TTL or explicit `DELETE`.
- **DNS-over-HTTPS as an exfil path.** Out of the proxy's reach at
  the transport layer; operator-side discipline on `allow_hosts`.
