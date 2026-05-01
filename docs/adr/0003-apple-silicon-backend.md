# ADR 0003: Apple Silicon backend via Virtualization.framework

Status: accepted, 2026-04-30
Phase: 4.5 (post-Phase-4, pre-Phase-5)

## Context

Mac contributors had two options for local Engram development:

1. **`engram-sandbox-process`** — subprocesses on the host. Fast iteration
   but no isolation, no kernel, no real microVM code paths. Useful for
   coordinator/scheduler work, useless for anything that touches the
   in-VM contract (init shim, agentd transport, snapshot semantics).
2. **`engram-sandbox-firecracker` on a remote Linux dev VM** (the
   `dev-vm` skill's GCP box). Faithful to production but adds remote-
   sync friction (Mutagen), an internet round-trip per `cargo test`,
   and ongoing GCP cost.

Neither was satisfying. Mac was a second-class dev target for any work
that touched the FC code paths — exactly the work that mattered most
(Phase 4 harness wiring, Phase 5 image bake, Phase 6 networking).

We considered three paths to fix this:

- **Run Firecracker on macOS.** Firecracker requires KVM. Apple's
  Hypervisor.framework is a different ABI; FC has no port and no
  upstream interest in one. Out.
- **A managed Linux VM on the Mac (UTM, Lima, OrbStack) running FC
  inside.** Three layers of virtualization (Apple HVF → Linux guest →
  KVM → FC microVM). Works, but every layer adds a config, every layer
  can drift, and nested KVM under Apple HVF is officially unsupported.
- **A second `SandboxBackend` impl that drives Apple's
  Virtualization.framework directly.** First-class isolation, single
  layer of virtualization, native macOS performance. The trait surface
  was designed for exactly this.

## Decision

**Build `engram-sandbox-vz` — a `SandboxBackend` impl driving Apple's
Virtualization.framework directly from Rust via `objc2-virtualization`.**

Mac dev gets real microVM isolation locally. The same `engram-bootstrap`
+ `engram-agentd` + harness adapter binaries cross-compile to
`aarch64-unknown-linux-musl` and run inside a VZ guest, exercising the
production code paths the way they'll run on a real Linux host.

**Five concrete sub-decisions** worth noting:

### 1. Pure Rust, no Swift driver

Earlier scaffolding sketched a Swift binary fronting Apple's APIs with
a JSON IPC to the Rust process. We rejected that: the Apple framework
calls are mostly straight Objective-C method invocations (start, stop,
pause, resume, save, restore), and `objc2-virtualization` exposes the
full surface as typed Rust bindings. The JSON IPC was solving a problem
the bindings already solve. One language, one process, one ABI to debug.

### 2. virtio-console for the host↔guest control plane (not vsock)

Firecracker's host↔guest control transport is virtio-vsock — the
universal default for microVM control planes. VZ supports vsock too,
but the **standard Linux kernels we boot under VZ don't ship
`CONFIG_VIRTIO_VSOCKETS=y`** in their kconfig (Ubuntu cloud-image, Kata
static, most distros). The in-VM `engram-agentd` panics on
`socket(AF_VSOCK) → EAFNOSUPPORT`, which kernel-panics init.

Two options: (a) build a custom kernel with vsock enabled (perpetual
maintenance overhead per VZ kernel update); (b) move VZ to
virtio-console for the control plane. Option (b) wins:

- `CONFIG_VIRTIO_CONSOLE=y` is universal; works on every kernel we
  might ever want to boot, no special-purpose kernel required.
- VZ's `VZVirtioConsoleDeviceConfiguration` exposes multi-port ports
  exactly like vsock — one port per logical channel (1024 = agentd,
  1025 = bootstrap, 1026 = harness adapter).
- Each port's host fd is given to us at VM-config time as an
  `NSFileHandle` (no `connectToPort` dial-then-pump dance, no
  `VZVirtioSocketListener` delegate); the host-side bridge is half
  the LOC of the vsock bridge.

FC stays on vsock. The two backends converge above the transport layer
via the new `engram-transport` crate — a `Transport` trait with
`VsockTransport` and `ConsoleTransport` impls; all four in-VM binaries
(`agentd`, `bootstrap`, `harness-noop`, `harness-claude`) dispatch on
`ENGRAM_TRANSPORT=vsock|console` at runtime, set by the bake's init
shim.

### 3. Clone-based snapshots, not VZ memory snapshots

VZ's `saveMachineStateToURL` / `restoreMachineStateFromURL` pair is
**broken upstream for arm64 Linux guests on macOS**. Save succeeds;
restore returns generic `VZErrorRestore=12` "invalid argument" with no
`NSUnderlyingError`. We confirmed against UTM's `#6654` issue, the
Apple Developer Forum thread `745168`, and the fact that **Apple's own
`containerization` framework avoids the API entirely for Linux** —
they use sub-second cold-boot instead. We tried every plausible
configuration (single-port console, frozen rootfs via APFS clone,
fresh-instance restore, sync-mode `None`,
`validateSaveRestoreSupportWithError`) — all failed identically. It's
not us; it's a real Apple-side limitation.

Our replacement: snapshot semantics are clone-based.

- **`snapshot()`** — pause VM → APFS-clone the per-sandbox rootfs into
  the snapshot dir → resume → write manifest. The clone *is* the
  snapshot. APFS `clonefile(2)` takes ~50 ms even for a 1.7 GB ext4
  rootfs (block-level COW; logically a copy, physically a reference).
- **`restore()`** — read manifest → APFS-clone snapshot rootfs into
  a fresh per-sandbox file → cold-boot a fresh VM. Bootstrap supervisor
  + `claude --resume <id>` carry conversation continuity across the
  cold boot. Hot resume isn't possible (no preserved memory state),
  but cold resume is sub-second.

Per-sandbox rootfs is required for this to be safe: each sandbox gets
its own `<work_dir>/<sandbox_id>.rootfs.ext4` (cloned from the bake's
warm-1 image at `create()`). Concurrent sandboxes no longer share a
writable disk.

### 4. Kata Containers static kernel, not Ubuntu cloud-image

The Ubuntu 24.04 cloud-image kernel boots in ~3-5s under VZ — its
kconfig assumes PCI/ACPI which VZ doesn't expose. The Kata Containers
static arm64 kernel — same one Apple's `containerization` CLI uses by
default — boots in ~600 ms: PCI/ACPI/USB/sound/graphics stripped out,
VIRTIO_BLK/NET/CONSOLE built in. Fetched via `just vz-pull-kernel`
which downloads the Kata release and extracts just the kernel
(~25 MB). Combined with kernel cmdline tuning
(`tsc=reliable panic=0 quiet`), measured cold boot is **562 ms**;
cold resume is **746 ms**.

### 5. Warm pool reuses the existing `PooledBackend` wrapper

The warm pool is a generic `SandboxBackend` wrapper that calls
`inner.create()` to fill the pool. It's already backend-agnostic — we
got VZ pool support for free, no VZ-specific code changes. Pool
checkout: ~30 ms. Same code path as FC, exercised the same way during
local Mac testing.

## Consequences

**Positive:**

- Mac dev is a first-class environment for FC code paths, not just for
  coordinator/scheduler work. The full lifecycle
  (`status_changed → run_started → harness_idle → snapshot_taken →
  evicted → idle → resumed → active`) runs end-to-end on a Mac, with
  measured sub-second timings.
- Same `engram-bootstrap` + `engram-agentd` + harness binaries run on
  both backends — exercising them on VZ catches regressions before
  they hit the Linux dev VM round-trip.
- VZ stops needing a special-purpose kernel forever (universal
  kconfig). Kernel updates are just `just vz-pull-kernel`.
- The Apple Silicon backend is faithful enough that we'd consider it
  for production *small-scale* deployments on Mac Studio / Mac mini
  fleets — though we're not committing to that yet.

**Negative:**

- **No hot resume on macOS.** VZ's broken save/restore means every
  resume is a cold boot from the cloned rootfs. Sub-second is fine for
  Mac dev; production needs FC's UFFD-backed hot resume.
- **One more backend to maintain.** New objc2 binding versions,
  Apple's WWDC 26 deprecations, and the next macOS release each become
  potential regression vectors. The compensating factor: the trait
  surface is small (`create`/`destroy`/`exec_stream`/`snapshot`/
  `restore`/`start_agent` + 14 unit tests), and the bridge is ~200
  LOC.
- **Codesigning is required.** Without the
  `com.apple.security.virtualization` entitlement, every VZ API call
  fails with NSError. `just vz-codesign` ad-hoc-signs the dev binaries;
  CI runs on `macos-15` arm64 with the same.
- **`engram-transport` is a new crate** to maintain. Two impls; each
  trivially small. The trait abstraction adds one indirection per
  `dial`/`listen` but lets future backends (microsandbox? a hosted
  Modal-style remote backend?) plug in without touching the in-VM
  binaries.

## Alternatives considered

- **Custom kernel with `CONFIG_VIRTIO_VSOCKETS=y` + vsock everywhere.**
  Would let VZ keep parity with FC's transport choice. Rejected:
  perpetual kernel-build maintenance, larger artifact, more chances
  for the kernel to drift from Apple's kconfig expectations. The
  virtio-console move is a permanent simplification.
- **Speak Apple Hypervisor.framework directly (one layer below VZ).**
  More control, more surface. Rejected: HVF is the lower-level API VZ
  itself is built on. VZ gives us boot loaders, virtio device classes,
  paravirtualized graphics for free; HVF would force us to reimplement
  all of that. We'd only reach for HVF if VZ's contract started
  fighting us — for now it doesn't.
- **Defer the Mac backend to Phase 7's "adopter experience" work.**
  Tempting but wrong-headed: Mac dev velocity *is* the contributor
  experience. Every regression caught locally on a Mac is one fewer
  Mutagen sync round-trip on the dev VM.

## Implementation notes

- `crates/engram-sandbox-vz` — ~1500 LOC. Modules: `vm` (VZ lifecycle
  wrapper), `console_bridge` (virtio-console host↔UDS pump),
  `disk` (APFS clone helper), `snapshot` (manifest format), `backend`
  (`SandboxBackend` impl).
- `crates/engram-transport` — ~300 LOC across `vsock.rs` + `console.rs`
  + the trait. `from_env()` factory dispatches on `ENGRAM_TRANSPORT`.
- `just vz-pull-kernel` — downloads Kata 3.17.0 arm64 kernel into
  `~/.cache/engram-vz-test/vmlinux-arm64`.
- `just vz-bake-{demo,claude}` — aarch64 cross-compile + `docker buildx
  --platform linux/arm64` + `mke2fs -t ext4 -F -d` (e2fsprogs from
  Homebrew, PATH-prepended).
- `just dev-vz` — codesigns + launches coord with the VZ backend.
  Default `ENGRAM_WARM_POOL_SIZE=1`, matching `dev-firecracker`.
- CI: `.github/workflows/ci-macos-vz.yml` runs the unit tests on
  `macos-15` arm64.
- Demo runbook: `docs/demo-vz.md`.
