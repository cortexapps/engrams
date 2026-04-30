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

The standard Ubuntu 24.04 cloud-image kernel works:

```bash
just vz-pull-ubuntu-kernel
# → ~/.cache/engram-vz-test/vmlinuz-arm64 (compressed; uncompress
#   to vmlinux-arm64 — VZ rejects gzip-wrapped kernels)
gunzip -k -c ~/.cache/engram-vz-test/vmlinuz-arm64 \
    > ~/.cache/engram-vz-test/vmlinux-arm64
file ~/.cache/engram-vz-test/vmlinux-arm64
# → "Linux kernel ARM64 boot executable Image"
export ENGRAM_VZ_KERNEL_PATH=~/.cache/engram-vz-test/vmlinux-arm64
```

Other working sources (any one works; pick whatever's already on
the machine):
- Apple's `containerization` framework's bundled kernel
- Lima / colima cached images
- A custom build from upstream sources

## Step 2 — bake the rootfs

```bash
just vz-bake-demo      # noop harness, ~230 MB
# or
just vz-bake-claude    # claude harness, ~400 MB. Requires
                       # ANTHROPIC_API_KEY at coord startup.
```

Both recipes:
1. Cross-compile `engram-{agentd,bootstrap,harness-*}` for
   `aarch64-unknown-linux-musl`.
2. `docker buildx --platform linux/arm64` build a debian-slim
   (or node:20-slim) base image with the binaries injected at
   `/sbin/engram-{agentd,bootstrap,harness-*}` plus the
   `/sbin/engram-init` shim.
3. Convert to ext4 via `mke2fs` (PATH-prepended from
   `/opt/homebrew/opt/e2fsprogs/sbin`).
4. Land at `./var/engram/images/local:/{demo|claude-demo}/warm-1/rootfs.ext4`.

VZ requires disk images to be 512-byte aligned. The bake pads
automatically; a stale unpadded ext4 will fail `create()` with VZ
NSError "Invalid disk image. The disk image format is not recognized."

## Step 3 — codesign + run

```bash
just dev-vz            # codesigns + launches coord with VZ backend
```

The `vz-codesign` recipe ad-hoc-signs `target/debug/engram-coordinator`
plus the `engram-sandbox-vz` test binaries with the
`com.apple.security.virtualization` entitlement. Without it, every VZ
API call fails with NSError "process doesn't have the
com.apple.security.virtualization entitlement" — caught by the
`vz_vm_new_full_plumbing_runs_or_fails_cleanly` smoke test in
`crates/engram-sandbox-vz/src/vm.rs`.

## Step 4 — exercise the lifecycle

```bash
SID=$(curl -sS -X POST http://127.0.0.1:8090/sessions \
        -H 'Content-Type: application/json' \
        -d '{"repo": "local://demo", "branch": "main",
             "image_version": "warm-1"}' \
      | jq -r .session_id)

# Watch the chat-shaped wire.
curl -N http://127.0.0.1:8090/sessions/$SID/events
# Expect:
#   status_changed (pending → active)
#   harness_idle
#   <agent activity if a prompt is fed>

# Send a prompt (claude bake only).
curl -sS -X POST http://127.0.0.1:8090/sessions/$SID/prompt \
    -H 'Content-Type: application/json' \
    -d '{"text": "list /workspace and tell me what you see"}'

# Idle eviction → hot auto-resume.
sleep 35       # crosses the 30s soft TTL
curl http://127.0.0.1:8090/sessions/$SID  # status: idle
curl -sS -X POST http://127.0.0.1:8090/sessions/$SID/prompt \
    -H 'Content-Type: application/json' \
    -d '{"text": "still there?"}'
# VZ restoreMachineStateFromURL fires; bridge re-binds; new harness
# adapter dials back.

# Force a Dead session (snapshot invalidation).
sleep 35
rm -rf var/engram/snapshots/$SID
curl -sS -X POST http://127.0.0.1:8090/sessions/$SID/prompt \
    -d '{"text": "this should fail"}'
# Expect HTTP 410 Gone with body "snapshot_invalidated".

curl -sS -X POST http://127.0.0.1:8090/sessions/$SID/fork
# Forks workspace + events into a new session; old conversation is not
# replayed (one-shot task runner contract).
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
- ✅ Snapshot/restore via `saveMachineStateToURL` /
  `restoreMachineStateFromURL`.
- ✅ Bake pipeline: aarch64 cross-compile → docker buildx →
  ext4 → 512-byte aligned.
- ✅ Codesign step.
- ✅ End-to-end demo runs on the standard Ubuntu cloud-image
  kernel.
