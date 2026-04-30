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
`VZLinuxBootLoader`. The kernel must have:

- `CONFIG_VIRTIO_VSOCKETS=y` (built-in, not a module)
- `CONFIG_VIRTIO_BLK=y`
- `CONFIG_VIRTIO_NET=y`
- `CONFIG_VIRTIO_CONSOLE=y`

**The Ubuntu 24.04 cloud-image kernel (`noble-server-cloudimg-arm64-vmlinuz-generic`)
does NOT have `CONFIG_VIRTIO_VSOCKETS=y`**. If you boot with it, the
in-VM `engram-bootstrap` fails with `Address family not supported by
protocol (os error 97)` and the kernel panics with
`Attempted to kill init`. Our `vz-pull-ubuntu-kernel` recipe is a
known-broken baseline kept for diagnostic purposes.

Working sources:

- **Apple's containerization sample kernel** — bundled with Apple's
  Containerization framework, baked specifically for VZ.
- **Lima / colima images** — `~/.lima/_images/` holds vsock-enabled
  kernels for Apple Silicon. Extract with
  `lima --debug` once a Lima VM has been created.
- **Build from source** — easiest if you have a Linux build machine.
  Standard arm64 defconfig + the four virtio configs above.

Cache the kernel at `~/.cache/engram-vz-test/vmlinux-arm64`. Must be
the uncompressed Image format (the `file` command should report
`Linux kernel ARM64 boot executable Image`). Compressed `vmlinuz` is
NOT accepted by VZ.

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
- **`vz vsock connectToPort failed`**: the in-VM listener for that
  port hasn't bound yet. The bridge retries with backoff for ~15s; a
  permanent failure usually means the kernel is missing
  `CONFIG_VIRTIO_VSOCKETS=y` (see Step 1).
- **Smoke test the plumbing without a real kernel**:
  `cargo nextest run -p engram-sandbox-vz` exercises VZ
  config-build through `validateWithError:`. The
  `vz_vm_new_full_plumbing_runs_or_fails_cleanly` test passes both
  with and without entitlement — its purpose is to catch
  regressions in the objc2 bindings, not the boot path.

## Status

- ✅ Coord wiring + `--sandbox-backend=vz` flag.
- ✅ VM lifecycle: create, start, stop, destroy.
- ✅ Vsock UDS bridge: ports 1024 (agentd), 1025 (bootstrap),
  1026 (harness) with full retry/backoff during kernel boot.
- ✅ Snapshot/restore via `saveMachineStateToURL` /
  `restoreMachineStateFromURL`.
- ✅ Bake pipeline: aarch64 cross-compile → docker buildx →
  ext4 → 512-byte aligned.
- ✅ Codesign step.
- ⚠️  End-to-end demo blocked on a vsock-capable arm64 kernel
  (Ubuntu generic kernel doesn't qualify — see Step 1).
