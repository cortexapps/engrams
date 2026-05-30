# Split-mode notes — the WS path between coordinator and host-agent

Since ADR 0024, `just dev` **is** the split topology: the Tiltfile runs
the coordinator in `mode=coordinator` and a separate `engram-host-agent`
process that dials it over HTTP+gRPC — the same wire the multi-host
production deployment runs. There's no longer a separate "split" recipe
or launcher; this doc just records what that path exercises and how to
drive a Claude session through the full network/proxy chain.

(Single-binary `mode=all` — the auto-degrade path `just dev` takes only
on a host with no KVM/VZ — does *not* touch the WS layer; bugs in the
wire protocol hide there.)

## What the split path exercises that an in-process backend doesn't

- **`HostClient` over the wire (ADR 0011):** every coord→host call is a
  bincode `Frame` over a real WebSocket — `CreateSandbox`, `StartAgent`,
  `BindHarnessSession`, `SendHarnessPrompt`, `GuestIp`, `Snapshot`,
  `Restore`, …
- **Harness event forwarding (ADR 0011):** the host-agent's `HarnessHub`
  ships each per-session event over the WS as a `Notify`; the coord's
  read loop re-emits via `state.emit` so SSE subscribers see the same
  stream `mode=all` would produce.
- **The filtering DNS proxy (ADR 0010):** iptables REDIRECT for `udp/53`
  + `tcp/53` lands at the host-agent's `engram-egress-proxy::dns`
  listener; `manifest.network.allow_hosts` decides allow vs `NXDOMAIN`.
- **The TCP/443 SNI proxy (ADR 0006):** reachable only because the
  host-agent's `SessionEgressPolicy` notify lands and registers the
  per-session state.
- **Heartbeat liveness:** WS heartbeats refresh `hosts.last_heartbeat_at`;
  without it the dead-host detector reaps every host ~90s after register.

## Bring it up

On the KVM dev-vm (see `docs/dev-vm.md` for the dev-vm skill setup):

```bash
# `just dev` runs coord + host-agent split, FC backend (in tmux: tilt up
# is long-running). Then bake the Claude image to the local registry.
bash .claude/skills/dev-vm/scripts/ssh.sh \
  "tmux new-session -d -s engram 'cd ~/engrams && nix develop --command just dev'"
bash .claude/skills/dev-vm/scripts/run.sh just bake-demo
bash .claude/skills/dev-vm/scripts/run.sh just integration-session  # HARNESS=claude to drive Claude
```

Dev runs with auth disabled, so API calls need no bearer token. Enable
the image + create/drive a session with `just integration-session`
(`HARNESS=claude PROMPT='…'`), or hit the coord API directly at
`http://127.0.0.1:8090`.

## The full Claude-through-the-filter chain

A Claude session against the `demo-claude` image (whose `engram.toml`
sets `network.allow_hosts = [..., "api.anthropic.com", ...]`) exercises
the whole split + proxy path. With a bogus `ANTHROPIC_API_KEY`, within
~30 s the SSE event stream surfaces an `agent_message` (`role=assistant`)
reading **"Invalid API key · Fix external API key"** — the artifact that
proves every hop worked:

1. Coord → host: `CreateSandbox` over WS.
2. Coord asks host for `GuestIp`; host returns the /30 guest IP.
3. Coord → host: `NotifyKind::SessionEgressPolicy(...)`; host's
   PooledBackend registers the policy in the egress proxy.
4. Coord → host: `StartAgent`; host's FC backend opens a vsock to
   `engram-agentd`, pushes a `SpawnHarness` frame; agentd exec's Claude.
5. Coord → host: `SendHarnessPrompt`; host's hub forwards into the in-VM
   adapter; Claude starts.
6. Claude DNS lookup `api.anthropic.com` → iptables REDIRECT → host DNS
   proxy → QNAME checked against `allow_hosts` → forwarded → response.
7. Claude TCP connect to the resolved IP:443 → iptables REDIRECT → host
   SNI proxy → SNI peek → bypass to upstream (ADR 0006).
8. Anthropic returns 401; Claude prints the auth error.
9. Harness wrapper translates it to a `HarnessEvent::AgentMessage`;
   host's hub fires the EventSink → `NotifyKind::HarnessEvent` over WS.
10. Coord's host read loop ingests → `state.emit` → `session_events`
    row → SSE bus → your `/events` stream.

## Test a denied DNS lookup

`getent hosts api.anthropic.com` inside the session resolves (exit 0 — in
`allow_hosts`); `getent hosts evil.exfil.com` returns empty / exit 2 —
the host log shows `DNS denied; responding NXDOMAIN … reason=NotInAllowList`.
(`reason=UnknownGuest` means the proxy had no `SessionEgressPolicy` for
the source IP — a wiring symptom that shouldn't appear on a current build.)

## Stale-TAP cleanup between restarts

Each FC session leaves a `tap-engr-XXX` DOWN with a route for the /30 it
owned; the host-agent doesn't reap them on shutdown, and the kernel can
pick a DOWN tap for new traffic, silently dropping packets. Between runs:

```sh
bash .claude/skills/dev-vm/scripts/ssh.sh '
for t in $(ip -br link show | awk "/^tap-engr-/{print \$1}"); do
  sudo ip link delete $t 2>/dev/null
done
sudo iptables -F; sudo iptables -t nat -F
sudo rm -rf ~/engrams/var/host-sandboxes/* 2>/dev/null
'
```

## What this doesn't exercise

- **Multiple hosts.** `just dev` is one host-agent. For the 2-host evac
  path use `ENGRAM_INTEG_TWO_HOSTS=1 just dev` (adds `host-agent-b`) +
  `just integration-evac-test`.
- **DNS-over-HTTPS as an exfil path.** Out of the proxy's reach at the
  transport layer; operator-side discipline on `allow_hosts`.
