# Demo runbook — Apple Silicon (`engram-sandbox-vz`)

The arm64 sibling of `docs/demo-firecracker-claude.md`. Drives Engram on
macOS Apple Silicon via Apple's Virtualization.framework instead of
Firecracker. Same wire surface (vsock UDS at
`<work_dir>/<sandbox>.vsock_*`), same chat-shaped event stream
(`/sessions/:id/events`), same idle-evict + auto-resume cycle —
different VMM.

## One-time setup

```bash
# Cross-toolchain for arm64 + x86_64 musl (for both VZ and FC bakes).
brew install musl-cross

# mke2fs for the ext4 rootfs conversion. Keg-only.
brew install e2fsprogs

# rust target. (vz-bake-* recipes do this automatically too.)
rustup target add aarch64-unknown-linux-musl
```

The `.cargo/config.toml` at the repo root wires the cross-linker; no
shell-profile munging required.

## Step 1 — kernel

`engram-sandbox-vz` boots an arm64 Linux kernel via VZ's
`VZLinuxBootLoader`. The kernel only needs:

- `CONFIG_VIRTIO_BLK=y` (rootfs)
- `CONFIG_VIRTIO_NET=y` (NAT egress)
- `CONFIG_VIRTIO_CONSOLE=y` (host↔guest control channels)

`CONFIG_VIRTIO_VSOCKETS` is *not* required. The host↔guest control
plane runs over multi-port virtio-console rather than vsock; the
in-VM binaries select the transport at runtime via
`ENGRAM_TRANSPORT=console`, which the bake's `engram-init` shim
sets automatically.

```bash
just pull-kernel   # detects VZ on Apple Silicon → fetches the Kata arm64 kernel
# → ~/.cache/engram-vz-test/vmlinux-arm64
file ~/.cache/engram-vz-test/vmlinux-arm64
# → "Linux kernel ARM64 boot executable Image"
```

This pulls the Kata Containers static kernel — the same kernel
`apple/container` (Apple's container CLI built on
Virtualization.framework) uses by default. It's Linux 6.12.28
with a VZ-tuned kconfig: PCI/ACPI/USB/sound/graphics stripped out,
`VIRTIO_BLK/NET/CONSOLE` built in. Cold-boots in well under a
second on Apple Silicon.

Why not the Ubuntu cloud-image kernel? It boots ~5x slower (it
expects PCI/ACPI which VZ doesn't expose) and previously failed
mysteriously at init time. Use the Kata kernel.

Other working sources (any one works; pick whatever's already on
the machine):
- Apple's `containerization` framework's bundled kernel
- Lima / colima cached images
- A custom build from upstream sources

## Step 2 — bake the rootfs

```bash
just bake-demo     # builds the Claude harness for arm64, bakes
                   # deploy/demo-claude/, pushes localhost:5001/demo-claude:warm-1
```

`bake-demo` (arch detected from the backend probe — `linux-arm64` on VZ):
1. Cross-compiles `engram-agentd` + `engram-harness-claude` for
   `aarch64-unknown-linux-musl` and downloads the matching `claude` CLI.
2. Publishes the harness artifact to the local registry and points the
   baker's catalog at it (so it builds from your tree, not GHCR).
3. `docker build --platform linux/arm64` a debian-slim base with the
   agent injected + the `/sbin/engram-init` shim; converts to ext4 via
   `mke2fs` (PATH-prepended from `/opt/homebrew/opt/e2fsprogs/sbin`).
4. Pushes `localhost:5001/demo-claude:warm-1` (the registry `just dev`
   runs). Enable it with `engram image enable …` or `just integration-session`.

VZ requires disk images to be 512-byte aligned. The bake pads
automatically; a stale unpadded ext4 will fail `create()` with VZ
NSError "Invalid disk image. The disk image format is not recognized."

## Step 3 — codesign + run

```bash
just dev               # detects VZ on Apple Silicon; codesigns + launches the stack
```

On macOS `just dev` builds, then ad-hoc-signs the coordinator +
host-agent with the `com.apple.security.virtualization` entitlement
before exec (the Tiltfile's serve_cmd chains `codesign.sh debug`).
Without the entitlement every VZ API call fails with NSError "process
doesn't have the com.apple.security.virtualization entitlement" — caught
by the `vz_vm_new_full_plumbing_runs_or_fails_cleanly` smoke test in
`crates/engram-sandbox-vz/src/vm.rs`. (The standalone `vz-codesign`
recipe still exists, used by `just vz-test`.)

## Step 4 — exercise the lifecycle

```bash
# Pre-req: enable an image (one-time):
curl -sS -X POST http://127.0.0.1:8090/api/enabled-images \
    -H 'Content-Type: application/json' \
    -d '{"image_uri": "localhost:5001/demo-claude:warm-1"}'

SID=$(curl -sS -X POST http://127.0.0.1:8090/sessions \
        -H 'Content-Type: application/json' \
        -d '{"image": "localhost:5001/demo-claude:warm-1",
             "harness": {"kind": "builtin", "name": "claude"}}' \
      | jq -r .session_id)

# Watch the chat-shaped wire.
curl -N http://127.0.0.1:8090/sessions/$SID/events
# Expect:
#   status_changed (pending → active)
#   harness_idle
#   <agent activity if a prompt is fed>

# Send a prompt.
curl -sS -X POST http://127.0.0.1:8090/sessions/$SID/prompt \
    -H 'Content-Type: application/json' \
    -d '{"text": "list /workspace and tell me what you see"}'

# Idle eviction → hot auto-resume.
sleep 35       # crosses the 30s soft TTL
curl http://127.0.0.1:8090/sessions/$SID  # status: idle
curl -sS -X POST http://127.0.0.1:8090/sessions/$SID/prompt \
    -H 'Content-Type: application/json' \
    -d '{"text": "still there?"}'
# Hot resume: VZ rootfs clone restored, bridge re-binds, harness
# adapter dials back, sub-second.

# ADR 0005 — cold-tier flush + cold resume.
sleep 35       # back to Idle
# Explicitly flush (Stage 5 admin endpoint; same primitive Stage 7's
# disk-pressure detector calls implicitly).
curl -sS -X POST http://127.0.0.1:8090/api/admin/sessions/$SID/flush
curl http://127.0.0.1:8090/sessions/$SID  # status: cold_evicted
# Resume from cold tier — downloads blob, untars, restores on any host.
curl -sS -X POST http://127.0.0.1:8090/sessions/$SID/prompt \
    -d '{"text": "still there after cold flush?"}'

# Force a Dead session (snapshot invalidation).
sleep 35
rm -rf var/engram/snapshots/$SID
# Also clear the cold copy from the blob backend; otherwise resume
# falls back to cold tier successfully.
ENGRAM_BLOB_BACKEND=local && rm -rf var/engram/blobs/engram/snapshots/*
curl -sS -X POST http://127.0.0.1:8090/sessions/$SID/prompt \
    -d '{"text": "this should fail"}'
# Expect HTTP 410 Gone with body "snapshot_invalidated".
```

## Diagnostic tips

- **Serial console**: stderr from the coord carries kernel boot logs +
  the `engram-init` shim's output. Set `ENGRAM_VZ_SILENCE_CONSOLE=1`
  to mute it for production-style runs.
- **`console pump ended with error`**: the host UDS connection
  closed mid-stream. Usually benign — the consumer (coord's
  start_agent / exec_stream) finished its work and disconnected.
- **`/dev/hvcN: open failed`** in guest logs: the kernel's
  virtio-console driver didn't enumerate the configured port. Run
  `ls /sys/class/virtio-ports/` from the guest to confirm port
  names; check that `CONFIG_VIRTIO_CONSOLE=y` is set.
- **Smoke test the plumbing without a real kernel**:
  `cargo nextest run -p engram-sandbox-vz` exercises VZ
  config-build through `validateWithError:`. The
  `vz_vm_new_full_plumbing_runs_or_fails_cleanly` test passes both
  with and without entitlement — its purpose is to catch
  regressions in the objc2 bindings, not the boot path.

## Status

- ✅ Coord wiring + `--sandbox-backend=vz` flag.
- ✅ VM lifecycle: create, start, stop, destroy.
- ✅ Multi-port virtio-console bridge: ports 1024 (agentd),
  1025 (bootstrap), 1026 (harness). Universal kernel support —
  no `CONFIG_VIRTIO_VSOCKETS=y` needed.
- ✅ Snapshot/restore via APFS clone of the rootfs. We do **not**
  use `saveMachineStateToURL`/`restoreMachineStateFromURL`; that
  pair is broken upstream for arm64 Linux guests on VZ (UTM #6654,
  Apple DevForum 745168, and Apple's own `containerization`
  framework avoids the API for the same reason). Instead, snapshot
  pauses the VM, APFS-clones the per-sandbox rootfs into the
  snapshot dir, resumes — the clone *is* the snapshot. Restore
  clones it back into a fresh per-sandbox rootfs and cold-boots a
  new VM. Bootstrap-as-supervisor + `claude --resume <id>` carry
  conversation continuity across the cold boot. APFS `clonefile(2)`
  takes ~50 ms even for a 1.7 GB rootfs, so the snapshot/restore
  pair stays sub-second.
- ✅ Per-sandbox rootfs. Each sandbox gets its own
  `<work_dir>/<sandbox_id>.rootfs.ext4` (cloned from the bake's
  warm-1 image at `create()`); concurrent sandboxes no longer
  share a writable disk.
- ✅ Chunked-OCI session create. The `PooledBackend` wrapper
  resolves a session's rootfs through the chunked-OCI image cache
  (NVMe → BlobStorage → OCI registry; ADR 0008) before delegating
  to VZ. Same wrapper as Firecracker — no VZ-specific changes were
  required. The earlier warm-pool path was retired with ADR 0008:
  chunked-OCI rootfs + canonical-memory restore makes cold start
  fast enough that pre-warming isn't worth the complexity.
- ✅ Bake pipeline: aarch64 cross-compile → docker buildx →
  ext4 → 512-byte aligned.
- ✅ Codesign step.
- ✅ End-to-end noop + Claude demos run on the Kata Containers
  static kernel. Verified flow on macOS Apple Silicon:
  `status_changed → run_started → agent_message → run_completed →
  harness_idle → snapshot_taken → evicted → status_changed →
  run_started (post-hot-resume) → ... → cold_evicted →
  cold_resumed → run_started (post-cold-resume)`. The cold-tier
  branches (ADR 0005 / Stage 5+) flow through the same harness +
  bridge wire surface as the hot path.
