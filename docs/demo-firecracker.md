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
- Harness-driven flow on Firecracker: in-VM `engram-bootstrap`
  listens on vsock 1025; the host's `start_agent` pushes a
  `BootstrapLaunch` frame; bootstrap exec's the harness from
  `/run/engram/harnesses/<name>/harness` (mounted from the
  read-only ext4 substrate attached as `/dev/vdb`); the harness
  dials AF_VSOCK CID=2 port=1026 back to a per-sandbox UDS the
  FC backend pre-bound; events land in `session_events` and
  drive the same `harness_hub.idle_sandboxes(ttl)` path used by
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

```
# 1. Bake the FC image with engram-agentd injected.
/dev-vm run just fc-bake-demo

# 2. Start Postgres on the VM.
/dev-vm run just db-up

# 3. Start the coordinator wired to FC. tmux because the VM
#    cgroup kills SSH-spawned background jobs on disconnect.
/dev-vm ssh "tmux new-session -d -s engram \
  'cd ~/engrams && /nix/var/nix/profiles/default/bin/nix develop --command \
   env ENGRAM_KERNEL_IMAGE_PATH=\$HOME/.cache/engram-fc-test/vmlinux-5.10.223 \
       ENGRAM_DEFAULT_IMAGE=warm-1 \
       just dev-firecracker > /tmp/engram-coord.log 2>&1'"

# wait for the build + boot
/dev-vm ssh "tail -f /tmp/engram-coord.log" &  # ctrl-c when you see "coordinator listening"

# 4. Drive a session.
/dev-vm ssh '
SID=$(curl -s -X POST http://localhost:8090/sessions \
  -H "content-type: application/json" \
  -d "{\"repo\":\"local://demo\",\"branch\":\"main\"}" | jq -r .session_id)
echo session: $SID

# Real microVM exec.
curl -s -X POST http://localhost:8090/sessions/$SID/exec \
  -H "content-type: application/json" \
  -d "{\"command\":\"uname -a; cat /etc/os-release | head -3\"}" --max-time 30 | jq

# Write a marker, then snapshot.
curl -s -X POST http://localhost:8090/sessions/$SID/exec \
  -H "content-type: application/json" \
  -d "{\"command\":\"echo phase4-was-here > /tmp/marker\"}" --max-time 30 | jq -r .stdout
curl -s -X POST http://localhost:8090/sessions/$SID/snapshot --max-time 90 | jq

# Evict (drops the live VM, keeps the snapshot).
curl -s -X DELETE http://localhost:8090/sessions/$SID/local --max-time 30

# Resume (UFFD-backed restore).
curl -s -X POST http://localhost:8090/sessions/$SID/resume --max-time 60 | jq

# Marker survives the round-trip.
curl -s -X POST http://localhost:8090/sessions/$SID/exec \
  -H "content-type: application/json" \
  -d "{\"command\":\"cat /tmp/marker\"}" --max-time 30 | jq -r .stdout
'
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
  -d "{\"image\":\"localhost:5001/engram/fc-claude:warm-1\",
       \"harness\":{\"kind\":\"builtin\",\"name\":\"claude\"}}" | jq -r .session_id)

# Idle eviction → hot auto-resume.
sleep 35       # crosses the 30s soft TTL (ENGRAM_IDLE_TTL_SECS)
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
   the flag, plus a `just fc-bake-demo` recipe.

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

## Networking + secrets architecture (post-Phase 6)

```
guest ──┐
        │ 1. TCP connect to <upstream-ip>:443
        ▼
   tap-engr-XXX  (host-side TAP, gateway IP .1, /30)
        │
        │ 2. iptables PREROUTING: REDIRECT to 127.0.0.1:9443
        ▼
   engram-egress-proxy (host)
        │ 3. SO_ORIGINAL_DST recovers the upstream IP
        │ 4. peer_addr → guest IP → SessionState lookup
        │ 5. Peek SNI from ClientHello
        │ 6. Decision per (manifest.network ∪ secrets.allow_hosts):
        │       Reject  → close
        │       Bypass  → splice (no MITM)
        │       Intercept → terminate TLS, substitute placeholders
        │                   from secret_bundle, re-encrypt to upstream
        ▼
   real upstream
```

Iptables only enforces hard isolation:
- DROP inter-VM (`-s 10.200/16 -d 10.200/16`)
- DROP VM→RFC1918 / link-local / loopback
- DROP VM→host-INPUT
- ACCEPT VM→DNS to 1.1.1.1
- REDIRECT VM→tcp/443 → proxy (when `--egress-proxy-port` set)
- DROP everything else (proxy is the only egress when enabled)
- MASQUERADE on POSTROUTING for return traffic

CA delivery: per-host CA persisted to
`<work_dir>/egress-proxy/ca.{pem,key}`; the harness substrate
builder stamps `ca.pem` into the substrate at
`/.engram-host/ca.pem`; the init shim appends to
`/etc/ssl/certs/ca-certificates.crt` and exports `SSL_CERT_FILE`
+ `CURL_CA_BUNDLE` + `REQUESTS_CA_BUNDLE` + `NODE_EXTRA_CA_CERTS`
so glibc, curl, requests, and Node all trust proxy-minted leaves
before user code runs.
