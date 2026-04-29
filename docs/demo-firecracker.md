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

What this does **not** exercise yet:
- Harness-driven flow (auto-noop, idle eviction,
  auto-checkpoint on Idle). `engram-bootstrap` (Phase 5) is the
  in-VM piece that launches the harness; until it lands,
  `FirecrackerBackend::start_agent` errors with `InvalidSpec`,
  so we run with `ENGRAM_DEV_AUTO_NOOP=` (off).
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
       ENGRAM_WARM_POOL_SIZE=0 \
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
  session, observe checkpoint push). Should "just work" against
  this stack since smart-bootstrap is `git fetch + reset` over
  the existing exec channel.
- Phase 5: `engram-bootstrap` inside the rootfs so harness-
  driven flow (auto-noop, idle eviction, auto-checkpoint) works
  on FC. Until then, idle eviction is a no-op for FC sessions.
- Snapshot size optimization: 4 GiB per snapshot is the
  default-memory ceiling. Sessions with smaller resource hints
  in `engram.toml` would produce smaller snapshots.
