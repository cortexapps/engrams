# Engram demo runbook — real Firecracker on the GCE dev VM

Sister to `docs/demo.md`, but this one runs against a real
microVM through `engram-sandbox-firecracker` on the
`engram-dev` GCE box. Validates the Phase 4 surface against the
production sandbox path: real KVM VM, baked rootfs, vsock-driven
exec via `engram-agentd`, UFFD-backed snapshot/restore.

What this exercises:
- VM lifecycle: `session create` → real Firecracker microVM →
  `session exec` over vsock.
- Snapshot lifecycle: `snapshot` → `evict_local` → `resume`.
  In-memory state is preserved across the round-trip — files
  written before the snapshot are still there after resume.

What this also exercises (after the harness path landed):
- Harness-driven flow on Firecracker: in-VM `engram-agentd`
  listens on vsock 1024; the host's `start_agent` pushes a
  `SpawnHarness` request; agentd's harness supervisor exec's
  the harness from `/run/engram/harnesses/<name>/harness`
  (mounted from the read-only ext4 substrate attached as
  `/dev/vdb`); the harness dials AF_VSOCK CID=2 port=1026 back
  to a per-sandbox UDS the FC backend pre-bound; events land
  in `session_events` and drive the same
  `harness_hub.idle_sandboxes(ttl)` path used by
  ProcessBackend. Idle eviction fires automatically after 60s
  without harness events. End-to-end coverage in
  `crates/engram-sandbox-firecracker/tests/harness_loopback.rs`.
- Per-VM hard-isolation networking: each sandbox gets its own
  `/30` from the engram pool (default `10.200.0.0/16`), a TAP on
  the host, and an iptables chain enforcing
  `manifest.network.allow_hosts`. No shared L2, no DHCP server,
  no host-LAN reachability from the guest. SHELL tab works.
  Coverage in `tests/network_provision.rs` (run with sudo).
- Auto-resume on the FC path: a session that idle-evicted
  comes back via FC's UFFD-backed snapshot restore on the
  next `exec`, transparent to the caller.

What this does **not** exercise yet:
- Cross-host migration. Single host, single coordinator.
- Split-mode (separate `--mode=coordinator` + `--mode=host` processes
  on the same VM). That's covered in
  [`docs/demo-split-mode.md`](./demo-split-mode.md) — same FC stack
  underneath, but exercises the WS path between coord and host
  including harness-event forwarding and the filtering DNS proxy.
- Git-session checkpoint push. Works in principle on this stack;
  add it after picking a real test repo.

## Prereqs (one-time)

The dev-vm skill provisions everything on a fresh box:
```
/dev-vm bootstrap-local      # local: install mutagen + ssh config
/dev-vm start
/dev-vm bootstrap-remote     # remote: nix, firecracker, /dev/userfaultfd, etc.
/dev-vm sync-start
```

Cache the FC test artifacts on the VM (kernel + ubuntu rootfs):
```
/dev-vm run bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh
```

## Run the demo

Bring the stack up and bake the image with the unified recipes — `just
dev` auto-detects KVM and runs the FC split topology (ADR 0024); there's
no FC-specific recipe anymore. Then exercise the VM lifecycle by hand.

```
# 1. Stack up via Tilt (coord + host-agent, FC). tmux because the VM
#    cgroup kills SSH-spawned background jobs on disconnect. Watch
#    http://localhost:10350 (port-forwarded) until resources are green.
/dev-vm ssh "tmux new-session -d -s engram \
  'cd ~/engrams && nix develop --command just dev > /tmp/engram-tilt.log 2>&1'"

# 2. Bake + enable the demo image, create a session. integration-session
#    prints the session id + ready-to-paste curls; grab SID from it.
/dev-vm run just bake-demo
/dev-vm run just integration-session    # set SID to the printed session id
```

Then exercise snapshot → evict → resume against `$SID` (these hit the
coord API directly; unchanged by ADR 0024):

```
# Real microVM exec.
curl -s -X POST http://localhost:8090/sessions/$SID/exec \
  -H "content-type: application/json" \
  -d '{"command":"uname -a; cat /etc/os-release | head -3"}' --max-time 30 | jq

# Write a marker, snapshot, evict (drop the live VM), resume (UFFD restore).
curl -s -X POST http://localhost:8090/sessions/$SID/exec \
  -H "content-type: application/json" \
  -d '{"command":"echo phase4-was-here > /tmp/marker"}' --max-time 30 | jq -r .stdout
curl -s -X POST http://localhost:8090/sessions/$SID/snapshot --max-time 90 | jq
curl -s -X DELETE http://localhost:8090/sessions/$SID/local --max-time 30
curl -s -X POST http://localhost:8090/sessions/$SID/resume --max-time 60 | jq

# Marker survives the round-trip.
curl -s -X POST http://localhost:8090/sessions/$SID/exec \
  -H "content-type: application/json" \
  -d '{"command":"cat /tmp/marker"}' --max-time 30 | jq -r .stdout
```

## Idle eviction → hot resume → cold flush → cold resume

The cycle ADR 0005 (`docs/adr/0005-disk-pressure-blob-tier.md`) is
designed around. Same wire surface as `docs/demo-vz.md:128-157` runs
on VZ; FC differs only in that hot resume uses memory.bin (or UFFD)
rather than an APFS clone.

```
/dev-vm ssh '
SID=$(curl -s -X POST http://localhost:8090/sessions \
  -H "content-type: application/json" \
  -d "{\"image\":\"localhost:5001/demo-claude:warm-1\",
       \"harness\":{\"kind\":\"builtin\",\"name\":\"claude\"}}" | jq -r .session_id)

# Idle eviction → hot auto-resume. The default soft TTL is 5 min
# (ADR 0039 follow-up #20) so it doesn't evict interactive sessions
# mid-conversation; this demo sets a short TTL so the sleeps below
# trip it. Run the coord/host with `ENGRAM_IDLE_TTL_SECS=30`.
sleep 35       # crosses the demo soft TTL (ENGRAM_IDLE_TTL_SECS=30)
curl http://localhost:8090/sessions/$SID  # status: idle
curl -s -X POST http://localhost:8090/sessions/$SID/prompt \
  -H "content-type: application/json" \
  -d "{\"text\":\"still there?\"}"
# Hot resume: FC loads memory.bin (or UFFD), bridge re-binds, harness
# adapter dials back, sub-second.

# ADR 0005 — cold-tier flush + cross-host cold resume.
sleep 35       # back to Idle
# Stage 5 admin endpoint; same primitive Stage 7's disk-pressure
# detector calls implicitly when host disk runs low.
curl -s -X POST http://localhost:8090/api/admin/sessions/$SID/flush
curl http://localhost:8090/sessions/$SID  # status: cold_evicted

# Resume from cold tier — downloads blob, untars, restores on any
# host with capacity. Snapshot-affinity scheduler picked the source
# host for hot; cold path is host-agnostic.
curl -s -X POST http://localhost:8090/sessions/$SID/prompt \
  -d "{\"text\":\"still there after cold flush?\"}"

# Force a Dead session (snapshot invalidation).
sleep 35
sudo rm -rf /var/lib/engram/snapshots/$SID
ENGRAM_BLOB_BACKEND=local sudo rm -rf /var/lib/engram/blobs/engram/snapshots/*
curl -s -X POST http://localhost:8090/sessions/$SID/prompt \
  -d "{\"text\":\"this should fail\"}"
# Expect HTTP 410 Gone with body "snapshot_invalidated".
'
```

## Bugs the demo surfaced (all fixed in this pass)

1. **`engram image build` couldn't bake an FC-usable image.** The
   CLI didn't expose `--inject-agent`, so produced rootfs.ext4
   files had no `engram-agentd` and the host couldn't `exec()`
   against the VM (no vsock listener inside the guest). Added
   the flag, plus a bake recipe (later unified as `just bake-demo`,
   ADR 0024).

2. **`FirecrackerConfig::with_kernel` default boot args missed
   `init=/sbin/engram-init`.** The image-baker writes
   `/sbin/engram-init` to exec the agent; without the kernel arg
   the kernel tried to boot debian's systemd (which we don't
   ship), the VM hung in early init, and exec timed out. Boot
   args now end with `init=/sbin/engram-init` so any
   Engram-baked image launches the agent automatically.

3. **`exec_stream` raced against guest boot.** The agent inside
   the guest takes a couple of seconds to bind on vsock 1024;
   if the host connects before then, FC closes the UDS
   (`read FC vsock CONNECT response: early eof`). Added a
   ~10s exponential-backoff retry inside `exec_stream` for the
   handshake-only error patterns; non-race errors bail
   immediately.

4. **`FirecrackerClient`'s 10s default timeout tripped on
   snapshot/create for our 4 GiB VM.** Snapshot duration scales
   with guest memory — every dirty page is flushed to memory.bin
   synchronously. The snapshot path now overrides to 60s; tune
   higher for production-sized VMs.

## Where to look next

- Smart-bootstrap on FC with a real Git repo (`git+https://...`
  session, observe checkpoint push). Should "just work" since
  smart-bootstrap is `git fetch + reset` over the existing exec
  channel; the egress proxy auto-allows the Git URL's host
  (auto-augmentation in `api/sessions.rs`).
- Snapshot size optimization: 4 GiB per snapshot is the
  default-memory ceiling. Sessions with smaller resource hints in
  `engram.toml` would produce smaller snapshots.
- VM-level proxy e2e test: the unit + iptables tests cover the
  proxy in isolation; a future test should exec curl from inside a
  real sandbox through the iptables REDIRECT and verify the proxy
  substituted the placeholder. Tricky because the test fake
  upstream needs a destination reachable through the post-REDIRECT
  proxy path (a host-side resolver indirection).
- Coordinator-level idle-evict + auto-resume integration test
  against the FC backend (today the test surface lives in
  `crates/engram-coordinator/tests/api.rs` against ProcessBackend;
  no FC variant). Building it requires a coord-with-FC fixture, a
  step beyond the existing `TestFixture` shape — deferred until
  there's a concrete regression to motivate the infrastructure.

## Networking + secrets architecture

```
guest ──┐
        │ 1. TCP connect to <upstream-ip>:443
        │    or DNS query to <any-resolver>:53
        ▼
   tap-engr-XXX  (host-side TAP, gateway IP .1, /30)
        │
        │ 2. iptables PREROUTING (-t nat) catches all egress:
        │      tcp/443 → REDIRECT to 127.0.0.1:9443
        │      udp/53  → REDIRECT to 127.0.0.1:5353
        │      tcp/53  → REDIRECT to 127.0.0.1:5353
        ▼
   engram-egress-proxy (host) — three listeners on one process:
   tcp/9443 (TLS-MITM):
        │ a. SO_ORIGINAL_DST recovers the upstream IP
        │ b. peer_addr → guest IP → SessionState lookup
        │ c. Peek SNI from ClientHello
        │ d. Decision per (manifest.network ∪ secrets.allow_hosts):
        │      Reject  → close
        │      Bypass  → splice (no MITM)
        │      Intercept → terminate TLS, substitute placeholders
        │                  from secret_bundle, re-encrypt to upstream
        │
   udp/5353, tcp/5353 (DNS filter, ADR 0010):
        │ a. hickory-proto parses the query, pulls the first QNAME
        │ b. peer source IP → SessionState (same Registry as TCP/443)
        │ c. QNAME matches network_allow ∪ secrets.allow → forward
        │    to 1.1.1.1 verbatim and ship the response back
        │ d. Otherwise → synthetic NXDOMAIN (EDNS OPT echoed when
        │    the query carried one)
        ▼
   real upstream (Anthropic API, GitHub, npm, 1.1.1.1, …)
```

Iptables only enforces hard isolation + the REDIRECTs that pin the
proxy as the *only* egress path:
- DROP inter-VM (`-s 10.200/16 -d 10.200/16`)
- DROP VM→RFC1918 / link-local / loopback
- DROP VM→host-INPUT (with explicit ACCEPTs first for the proxy's
  TCP/443 listener and its DNS port — see `engram-sandbox-firecracker::net::host_startup_lines`)
- REDIRECT VM→tcp/443 → proxy (when `--egress-proxy-port` set)
- REDIRECT VM→udp/53, tcp/53 → proxy DNS port (default 5353)
- DROP everything else (proxy is the only egress when enabled)
- MASQUERADE on POSTROUTING for return traffic
- (No-proxy mode keeps the legacy `ACCEPT VM→1.1.1.1:53` rules so
  DNS still works when the operator opts out of filtering.)

CA delivery: per-host CA persisted to
`<work_dir>/egress-proxy/ca.{pem,key}`; the harness substrate
builder stamps `ca.pem` into the substrate at
`/.engram-host/ca.pem`; the init shim appends to
`/etc/ssl/certs/ca-certificates.crt` and exports `SSL_CERT_FILE`
+ `CURL_CA_BUNDLE` + `REQUESTS_CA_BUNDLE` + `NODE_EXTRA_CA_CERTS`
so glibc, curl, requests, and Node all trust proxy-minted leaves
before user code runs.
