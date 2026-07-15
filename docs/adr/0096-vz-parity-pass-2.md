# ADR 0096: VZ ↔ FC parity, pass 2 — close the drift, harden the loop

Status: 2026-07-15 — **Accepted.** Landed as one PR (single-PR by explicit
choice for this pass); every decision below shipped and is pinned by the
live `just vz-e2e` suite (8/8 booting-VM tests green on an M-series dev
machine, on the owned kernel). See § Commit chain. Two follow-ups remain
open: (1) the arm64 kernel release asset must be published by dispatching
`build-fc-kernel.yml` once (a locally-built binary was deliberately NOT
uploaded to the shared release); until then `just pull-kernel` 404s on
machines without a cached kernel. (2) The D7 spike answered NO — Apple's
machine-state restore is still broken on macOS 26 (see snapshot.rs) — so
the D7 productization path stays closed.

Builds on ADR 0003 (the VZ backend), ADR 0024 (unified dev orchestration), and
ADR 0032 (parity pass 1, June 2026). Supersedes ADR 0061's "a custom VZ kernel is
far more than the feature warrants" rationale (§ Kernel below). Production stays
the design driver ([[feedback-production-drives-design]]): every change here makes
VZ *exercise* the FC path, never fork it further.

## Context

ADR 0032 made the enable → create → exec → shell → snapshot → resume lifecycle work
on VZ. Six weeks of prod-driven work since (substrate v2b, the NBD data plane,
ADR 0093's streaming packer, ADR 0095 peer fill, forge-broker maturity) drifted the
macOS story again. A fresh survey (2026-07-15) — trait-level, runtime-level, and
hands-on on an M-series dev machine — found the drift falls into three buckets:

1. **Silently-broken parity that looks green.** The live `e2e_vz_*` tests skip
   when `ENGRAM_VZ_ROOTFS` is unset — in CI *and* in the default local flow — so
   `just vz-test` passes vacuously. Worse, the flow can't go live even
   deliberately: `vz-codesign` signs test binaries that `cargo nextest run` then
   recompiles unsigned (feature unification differs between `cargo build -p a -p b
   --tests` and nextest's invocation), and `vz-test`'s `-- --ignored` is a test
   *name filter* in nextest, not a libtest flag. And structurally: post-ADR-0080,
   agentd rides a reserved bundle slot; the e2e spec attaches no bundles, so the
   init shim exits and the kernel panics `Attempted to kill init!` — **no rootfs
   the repo can produce today boots the live test.**
2. **Unwired seams that no-op silently.** The forge broker's vsock port 1028 is
   not registered in the VZ vsock bridge (in-guest credential brokering dies with
   connection-refused retries); the trait-default `pause`/`resume` make the
   coordinator's rung-2 park a silent no-op; `read_total_memory_mib()` reads
   `/proc/meminfo` and returns 0 on Darwin, which makes the idle-evictor's
   headroom gate fail closed (park can never fire) and leaves the scheduler blind;
   there is no `VZVirtualMachineDelegate`, so a crashed guest is never eagerly
   reaped.
3. **Deliberate forks whose premises expired.** VZ boots the Kata static kernel
   instead of the ADR 0025 owned config — which forced the erofs bundle fork
   (Kata lacks `CONFIG_SQUASHFS`, ADR 0061) and blocks any in-guest netfilter
   work. Egress is wholly unenforced on VZ (`allow_hosts` warned-and-ignored), so
   the ADR 0056 egress-proxy inject/observe plane is untestable on local dev even
   though the proxy itself runs on macOS.

What is *not* drifted: VZ runs behind the same `PooledBackend` as FC — chunk
store, blob upload, materialize, snapshot durability, skills staging, the
agentd/harness/vsock control plane, shell/browser/IDE all exercise the shared
path. The forks are the memory/disk data plane and isolation.

## Decisions

### D1 — capability-probed backend fallback (`detect-backend.sh`)

Platform detection becomes capability probing. FC requires `/dev/kvm` present and
**read-writable**; VZ requires macOS/arm64 **and** `sysctl -n kern.hv_support` = 1
(a Mac VM without nested virtualization — an engrams session, some CI runners —
must not select VZ). When neither backend can actually run, detection degrades to
`process`, which forces the combined `--mode=all` distribution (the Tiltfile's
`dev_split = backend != 'process'` already encodes this; it is now the contract,
not a convenience). Diagnostics go to stderr; stdout stays one word. ADR 0024's
table is updated to match.

### D2 — ProcessBackend is un-split by doctrine

`--mode=all` is the Process backend's *only* home. The host-agent grows no
`BackendChoice::Process`; the coordinator's enable flow grows no tolerance for
manifest-less snapshots. The split arms of `mode=all` were deleted (#530 item f)
because they drifted; we affirm the inverse for Process: it exists to run the
product plane (coordinator API, scheduler, harness protocol) without
virtualization, with zero isolation and zero data-plane parity — and that is the
whole contract. The previously-floated "wrap Process in `PooledBackend`" ambition
is dead, not deferred.

### D3 — the live VZ loop must be self-staging and un-skippable

- A **Docker-free test rootfs**: `engram-mk-test-rootfs` (a small bin beside the
  materializer) assembles a pinned static busybox+socat tree, injects the *real*
  prod init shim (`inject_init`), and packs it with `stream_pack::pack_tree`
  (ADR 0093's mkext4 — pure Rust, runs on macOS). No Docker, no mounts, no root.
- The e2e spec attaches the reserved agentd + guest-tools bundle slots and
  resolves them from the bundle-dir stamp exactly like real sessions
  (post-ADR-0080 there is no other way to boot).
- `just vz-e2e` stages everything from HEAD (kernel, bundles, rootfs, codesign)
  and runs the live suite in one command — the loop can no longer rot silently.
- `ENGRAM_VZ_REQUIRE=1` turns preflight SKIPs into failures; CI sets it, so the
  lane can never regress back to skip-and-green. If the macOS CI runner cannot
  nest Virtualization.framework VMs (open question, probed by D1's `kern.hv_support`
  check), `just vz-e2e` is the documented local pre-merge gate instead, recorded
  here.

### D4 — wire the silent no-ops

- **Forge (vsock 1028):** the VZ bridge registers a 1028 listener and `VzBackend`
  overrides `set_forge_sink`, mirroring upload/1029 byte-for-byte. In-guest
  `engram-agentd forge-credential` then works on VZ; a forge-loopback e2e pins it.
- **Pause/resume:** `VzBackend` overrides the trait methods onto `VzVm`'s
  existing queue-dispatched `pause()`/`resume()`, made idempotent (the trait's
  documented FC contract). A live test pins vsock-survives-resume (ADR 0074's
  lesson: never assume it). With D5's capacity fix, rung-2 park then fires on VZ.
- **Capacity:** `read_total_memory_mib()` gets a Darwin arm via
  `sysctlbyname("hw.memsize")`.
- **Crash detection:** a retained `VZVirtualMachineDelegate` shim sets a dead
  flag on `guestDidStopVirtualMachine:`/`didStopWithError:`; `list()` filters
  dead sandboxes (the ADR 0009 heartbeat then reflects ground truth) and
  `probe_sandbox` reports `process_alive` from the live VM state, not map
  membership. Detection is eager; cleanup stays coordinator-driven (FC posture).
  This supersedes the "no delegate by design" note that lived in `backend.rs`.

### D5 — own the VZ guest kernel; retire the erofs fork

ADR 0061 chose erofs because building a kernel "far exceeded" one feature. The
premise expired: ADR 0082 already builds our config for arm64
(`build-fc-kernel.sh ARCH=arm64`), and kernel ownership is now load-bearing for
three things at once — squashfs (retiring the erofs fork and `bundles-vz`),
netfilter (D6), and config parity with prod. A `vz` variant (fragment:
`CONFIG_SQUASHFS`, virtio console/vsock built-in, `IP_PNP_DHCP`, plus whatever a
go/no-go boot of the existing arm64 asset under VZ reveals) publishes through the
same release-asset flow; `pull-kernel.sh` fetches it instead of the Kata tarball.
Erofs is removed as a clean break (bundle ext returns to the shared squashfs
default; developers restage bundles once; pre-flip VZ snapshots pinning `.erofs`
don't survive — acceptable, dev-only).

Permanent divergence recorded: there is no KVM-PTP device under Apple's
hypervisor — agentd's resume clock-step stays FC-only (VZ cold-boot semantics
don't need it).

### D6 — steer VZ egress through the proxy (soft enforcement)

Goal stated honestly: exercise the egress-proxy plane (interception, CA,
ADR 0056 placeholder substitution, violation reporting) on local dev. This is
**not** hard isolation — the guest is root and can flush its own rules; macOS NAT
stays open underneath. Mechanism: the kernel cmdline carries
`engram.egress=<proxy_port>:<dns_port>`; the init shim's VZ branch derives the
gateway from the guest's default route and installs the same REDIRECT-shaped
rules FC applies host-side (tcp/443 → proxy, 53 → dns). The static
iptables binary rides the guest-tools bundle (no per-image bake). Host side, the
proxy binds the vmnet-reachable address on VZ. The `allow_hosts`-ignored warning
narrows to a soft-enforcement note. Requires D5 (netfilter config).

### D7 — machine-state snapshot spike (timeboxed)

ADR 0003 chose clone+cold-boot because Apple's
`saveMachineStateToURL`/`restoreMachineStateFromURL` failed for arm64 Linux
guests (VZError 12, macOS-14 era). The plumbing still exists (`VzVm::save`/
`restore`, dead-code). On macOS 26 we re-validate once, sweeping the two
never-ruled-out causes: no explicit `VZGenericPlatformConfiguration` with a
persisted `VZGenericMachineIdentifier`, and per-device save/restore support
(console/NAT attachments). Either outcome is recorded in `snapshot.rs`'s header
with the macOS version. If green, productization (memory manifest, warm restore,
honest park, two-phase-eviction eligibility) is a *future* ADR — not this pass.

## Out of scope (permanent divergences, not regressions)

UFFD lazy restore, base-shm density, dirty-page diff snapshots, the NBD chunked
data plane, peer fill, live migration/post-copy: all Linux/KVM kernel primitives.
ADR 0082 (FC-via-colima) is the escape hatch when a Mac must exercise those.
`patch_drive` live bundle swap: Apple VZ has no block-device hotplug; VZ's
cold-boot re-attach is the honest equivalent. `guest_memory_stats` stays `None`
on VZ for now (all VZ guest memory is private — the density ratio it feeds is
meaningless there); the cheapest honest version (an agentd `MemInfo` RPC summing
guest-internal usage) is specced in the survey if ever wanted.

## Verification

- `just vz-e2e` green on a dev Mac: live boot, exec, snapshot → cold-boot
  restore, port relay, forge loopback, pause/resume.
- `just dev` full lifecycle on VZ including a forge-brokered git op; rung-2 park
  observed firing and un-parking.
- CI vz lane runs `e2e_vz_*` non-vacuously (`ENGRAM_VZ_REQUIRE=1`); unsetting the
  rootfs in a test commit fails the lane rather than skipping.
- Kernel flip: VZ cold-boot time measured before/after (no inner-loop
  regression); the skill-attach e2e passes against squashfs bundles.

## Commit chain

One PR, in landing order:

1. `dev: capability-probe the backend fallback` — D1 (detect-backend.sh
   probes `/dev/kvm` rw + `kern.hv_support`; degrade → process/mode=all).
2. `vz: fix the codesign/run contract` — the nextest-shape build, the nix
   `file(1)` word-order bug that signed NOTHING in the dev shell, native
   `--run-ignored`.
3. `vz: re-resolve symbolic bundle slots on restore` — the resume
   cold-boot booted bundle-less and panicked init.
4. `vz: serve the forge broker on vsock 1028` — D4; forge_delivery
   mirrors upload byte-for-byte.
5. `vz: self-staging live e2e — 'just vz-e2e'` — D3 (mk-test-rootfs via
   mkext4 + Alpine minirootfs, vz-test-bundles, ENGRAM_VZ_REQUIRE,
   doc-rot sweep).
6. `host-agent: real memory capacity on macOS via hw.memsize` — D4
   (unblocks the rung-2 headroom gate).
7. `host-agent: substrate populate server only on FC` — hygiene.
8. `ci: live VZ e2e in the vz lane when the runner has a hypervisor` —
   D3 (kern.hv_support probe; loud skip otherwise).
9. `vz: real external pause/resume` — D4 (idempotent VzVm pause/resume +
   state(); parked snapshot neither flushes nor un-parks).
10. `vz: crash detection via a VZVirtualMachineDelegate shim` — D4
    (dead flag; list() ground truth; probe_sandbox override).
11. `vz: machine-state save/restore spike` — D7 outcome: STILL broken on
    macOS 26 with a pinned machine identifier and a validator-approved
    config; probe stays in-tree.
12. `vz: own the guest kernel; retire the erofs bundle fork` — D5
    (Image-engram arm64 boots VZ as-is; bundles-vz deleted; squashfs
    everywhere; build-fc-kernel.yml arch matrix).
13. `vz: soft egress steering` — D6 (ENGRAM_EGRESS cmdline → init-shim
    DNAT to the proxy at the NAT gateway; SNI-dial compatible; soft by
    design and said so).
