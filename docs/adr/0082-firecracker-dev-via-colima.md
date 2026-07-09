# 0082 — Local Firecracker dev on Apple Silicon via a dedicated Colima VM

Status: Accepted (2026-07-09) — implemented on PR #595 (`fc-colima-dev`); the
phase log below records the P0–P3 divergences. Sibling decision: ADR 0083
(fail-closed egress-proxy bind), found via this rig and landed in the same PR.

## Context

Production isolation is Firecracker on Linux+KVM; macOS development uses the VZ
backend (ADR 0003) against the same coordinator code paths. That parity is good
but not complete: the FC-only surfaces — netns/netlink provisioning, the NBD
chunked-disk daemon, UFFD snapshot restore, squashfs bundle patch-drives, the
egress iptables chain — cannot run on macOS at all today. Exercising them means
the GCP `engram-dev` box (`docs/dev-vm.md`) or CI's `test-firecracker` lane.

Two things changed:

1. **Apple Silicon nested virtualization** (M3+, macOS 15+) gives a Linux VM a
   real `/dev/kvm`. Issue #569's forensics rig proved the full recipe locally:
   a Colima profile (`fc-dev`, `--vm-type vz --nested-virtualization`) boots
   upstream Firecracker v1.16.0-aarch64 guests running our 6.1.102 kernel
   recipe (`docs/runbooks/local-firecracker-colima.md` on the #569 branch).
2. Exploration confirmed the host-side FC code is architecture-agnostic: no
   `cfg(target_arch)` gates in `engram-sandbox-firecracker`, `engram-uffd-handler`
   (which already handles aarch64 page sizes, `runtime.rs`), or the host-agent.
   The bake pipeline is likewise already arch-generic for the VZ path:
   `deploy/dev/bake-demo.sh` bakes an aarch64 ext4 rootfs + musl agentd on any
   arm64 Mac, and the squashfs bundle builders arch-switch cleanly.

**The caveat that shapes everything:** nested virt is same-architecture only.
This rig runs an *aarch64 analog* of production — same kernel version + engram
fragment, same FC release train, same code paths — not the prod x86_64
binaries. x86_64 CI remains the authority; this is an inner-loop tool.

### What's missing today

- **No aarch64 FC guest kernel anywhere.** `deploy/kernel/build-fc-kernel.sh`
  hardcodes the vendored `microvm-kernel-ci-x86_64-6.1.config`; the GH release
  asset (`vmlinux-engram-6.1.102-1`) and the test-rootfs URL in
  `fetch-fc-test-artifacts.sh` are x86_64-only.
- **No way to point `just dev` at a KVM VM.** The Tiltfile launches the
  host-agent as a local Mac process; `detect-backend.sh` on macOS picks `vz`.
- **Loopback literalism.** Dev binds coordinator (`127.0.0.1:8090`) and
  host-agent gRPC (`127.0.0.1:9101`) to the Mac's loopback; image refs are
  `localhost:5001/...` and `engram-oci` allows plaintext HTTP only for literal
  loopback hosts (`is_loopback_host`).
- Small arch gates: `deploy/dev/integration-{bake-demo,session}.sh` hardcode
  the x86_64 musl agentd path; `just bake` rejects `Linux aarch64`.

## Decision

Add a first-class dev mode that runs **only the host-agent (and its FC stack)
inside a dedicated Colima VM**, while the coordinator, orchestrator, web, and
the docker-compose deps stay on the Mac exactly as in plain `just dev`.

Invocation: `just dev-fc [profile]` (default profile `fc-dev`), sugar for
`ENGRAM_FC_COLIMA_PROFILE=<profile> tilt up`. The env var is the real switch
(ADR 0024 style — the Tiltfile consumes it); persisting it in `.env` makes
plain `just dev` equivalent. The dedicated profile leaves the user's default
Colima docker daemon untouched.

Provisioning is one idempotent command: `just fc-colima-provision [profile]` →
`deploy/dev/fc-colima-provision.sh`, which:

- creates/starts the profile (`--vm-type vz --nested-virtualization`, M3+ gate),
  **capturing and restoring the docker CLI context** (`colima start` steals it);
- installs VM packages (kernel build deps, iptables, squashfs-tools),
  the upstream `firecracker` aarch64 release binary (same version CI pins);
- applies host prep mirroring CI/node-prep: `modprobe nbd nbds_max=…`
  (without it, base-snapshot capture at image-enable 500s — the ADR 0024
  gotcha), `vm.unprivileged_userfaultfd=1`;
- builds the aarch64 guest kernel **inside the VM** from
  `deploy/kernel/build-fc-kernel.sh`, which gains `ARCH` support (arm64 base
  config vendored alongside the x86_64 one; output `arch/arm64/boot/Image`).
  Publishing a CI-built aarch64 kernel asset is a follow-up, not part of this
  change;
- installs the loopback-forwarding units described below.

### Networking: preserve loopback semantics instead of rewiring addresses

Two directions, two mechanisms, zero changes to address strings where possible:

1. **Mac → VM (coordinator dials host-agent gRPC; every RPC, shell byte, and
   preview byte).** The host-agent binds `0.0.0.0:9101` inside the VM; Lima's
   guest-agent auto-forwards guest listeners to the Mac's `127.0.0.1`. The
   advertised address stays `http://127.0.0.1:9101` — the coordinator cannot
   tell the host-agent moved.
2. **VM → Mac (register/heartbeat HTTP, OCI registry pull, GCS-emulator
   chunks).** Lima guests reach Mac-loopback services via the host gateway
   (`192.168.5.2`). The coordinator endpoint is set to
   `http://192.168.5.2:8090` directly. The registry and GCS emulator instead
   get an **OUTPUT DNAT inside the VM** (`localhost:{5001,4443,4317} →
   gateway:PORT`, via `route_localnet` + MASQUERADE) so that the image refs
   baked into the DB (`localhost:5001/...`) and `engram-oci`'s loopback-only
   plaintext allowance keep working *unmodified*. We deliberately do not widen
   the OCI client's insecure-HTTP heuristic. (This started as socat listeners on
   `127.0.0.1:PORT`, but Lima auto-forwards guest loopback listeners back to the
   Mac, where they shadow the real deps — see Phase log P3. DNAT has no listener
   to shadow.)

### Tilt wiring

When `ENGRAM_FC_COLIMA_PROFILE` is set, the Tiltfile:

- forces `sandbox_backend = 'firecracker'` (the Mac-side `detect-backend.sh`
  probe is skipped; the VM has the `/dev/kvm`), which already routes bundles to
  `bundles-squashfs`;
- builds the host-agent + uffd-handler for the VM — cross-compiled to
  static `aarch64-unknown-linux-musl` from the Mac via `nix develop`
  (spike-verified: ~2.5 min cold, the flake's musl toolchain provides the
  Linux UAPI headers bindgen needs; no Rust toolchain in the VM at all);
- syncs the staged bundle dir (`var/shared`) and binaries into the VM's own
  disk (never serve FC block-device backings over virtiofs), then launches the
  agent via `colima ssh --profile <p> -- sudo -n …` with the same env contract
  the local path uses, plus `ENGRAM_KERNEL_IMAGE_PATH` → the VM-built Image and
  `ENGRAM_FIRECRACKER_BIN` → the provisioned binary.

### Also in scope

- Arch-detect fixes for `integration-{bake-demo,session}.sh` (mirror
  `bake-demo.sh`'s `uname -m` switch) and a `Linux aarch64` branch in
  `just bake`, so `just integration-session` works against the VM.
- A runbook: `docs/runbooks/fc-colima-dev.md`.

## Consequences

- FC-only surfaces (NBD chunked disks, UFFD restore, netns egress, squashfs
  patch-drives, base-snapshot capture) become exercisable in the Mac inner
  loop for the first time; `just integration-session` runs the real FC path.
- The rig is an aarch64 analog. Anything x86_64-specific (CPU templates, the
  prod FC fork binary, the prod kernel binary) is out of its reach; CI's KVM
  lane and the GCP dev-vm stay authoritative.
- VZ remains the zero-setup default; this mode is opt-in and additive. The
  two backends keep independent base snapshots per image (snapshot manifests
  are backend-specific by design), so switching modes re-captures.
- New moving parts owned by the provision script: the `engram-dev-fwd` DNAT
  unit (P3 replaced the original socat units), the Lima auto-forward
  assumption, and sysctl/modprobe state — all pinned in one idempotent script
  rather than scattered shell history.

## Phase log

- P0: exploration + spikes. Both networking assumptions verified against the
  live `fc-dev` VM (gateway→Mac-loopback via `192.168.5.2`; guest-listener
  auto-forward to the Mac's `127.0.0.1`). Cross-compile verified (static musl
  binaries for `engram-host-agent` + `engram-uffd-handler`).
- P1: implementation. Divergences and pitfalls found on the way:
  - **`colima ssh` propagates no signals** (verified live: SIGTERM/SIGHUP/
    process-group kill of the local wrapper never reach the remote process,
    and there is no pty flag). So Tilt restarts rely on a pre-kill step
    (`pkill -f` before each launch), and `tilt down` leaves the remote agent
    and its microVMs running until the next start or a manual pkill —
    documented in the runbook.
  - **`mksquashfs` on macOS comes from the flake** (`squashfsTools` is an
    unconditional devShell package), so the `bundles` resource wraps
    `just bundles-squashfs` in `nix develop -c` on Darwin — no Docker detour.
  - **Stale `just bake` transport bug found**: the Darwin branch still passed
    `--transport console`, which ADR 0066 retired (the variant is deleted;
    console-baked images can't boot). Fixed to vsock-always, and a
    `Linux aarch64` host branch added.
  - Pre-existing Tiltfile bug fixed in passing: `fail(a + b .format(...))`
    bound `.format()` to only the last string literal, so the kernel-missing
    message printed raw `{path}` placeholders.
  - `ENGRAM_INTEG_TWO_HOSTS` + this mode is rejected at parse time (a second
    agent would need its own forwarded ports and NBD split inside the VM).
- P2: **docker-context steal reaches `dev-fc`, not just provisioning.**
  `colima start` doesn't just transiently steal the docker CLI context — it
  writes `currentContext=colima-<profile>` into `~/.docker/config.json`, which
  is sticky for every later shell. `fc-colima-provision.sh` captures/restores it
  (P1), but `just dev-fc` was a bare `ENGRAM_FC_COLIMA_PROFILE=… tilt up` with no
  restore, so whenever the profile had been (re)started outside the provision
  wrapper (reboot/auto-start/manual `colima start`), Tilt inherited the VM
  context and `docker_compose()` deployed postgres/registry/fake-gcs **into the
  VM**. There the registry can't even bind — the in-VM socat forwarder already
  owns `127.0.0.1:5001` (`0.0.0.0:5001` overlaps it) — and postgres only "works"
  by colima re-forwarding its published port back to the Mac, which made
  `just db-reset` wipe a DB the still-running coordinator couldn't see. Fix:
  `dev-fc` now pins `DOCKER_HOST` to the Mac docker for the tilt process only
  (resolved from a `mac_docker_context` param, default `colima`; no global
  mutation), and the Tiltfile `fail()`s fast if the effective docker host is the
  `colima-<profile>` VM socket — so the deps can never silently land in the VM
  again (a direct `tilt up` under the stolen context is now a clear error, not a
  half-broken stack).
- P3: **the socat loopback forwarders shadow the Mac deps once the deps are
  correctly on the Mac.** The §Networking design used per-service socat units
  listening on the VM's `127.0.0.1:{5001,4443,4317}` and forwarding to the
  gateway. But Lima's default `portForwards` includes a blanket `guestIP:
  127.0.0.1 → hostIP: 127.0.0.1` rule, so it surfaces those socat listeners onto
  the *Mac's* loopback — where `127.0.0.1:5001` beats the deps' `0.0.0.0:5001`
  forward and shadows the real registry. Every Mac-side `localhost:5001` (bake
  push, coordinator dial) then loops back into the VM and fails with an empty
  reply. It stayed hidden only because the deps had been (wrongly) in the VM, so
  `localhost:5001` resolved locally and the socat/shadow never mattered — the P2
  context-pin, by moving the deps onto the Mac as intended, is what exposed it.
  colima 0.9.x regenerates `lima.yaml` on start and exposes no `portForwards`
  passthrough, so the ignore-rule route is unavailable. Fix: replace the socat
  units with an OUTPUT **DNAT** (`127.0.0.1:PORT → gateway:PORT`, `route_localnet`
  + MASQUERADE) — no loopback *listener*, so Lima has nothing to surface, while
  the guest's own `localhost:PORT` still reaches the Mac dep and engram-oci's
  loopback-plaintext allowance still applies (the ref string is unchanged).
  `fc-colima-provision.sh` now installs `engram-dev-fwd.service` (a boot-time
  oneshot running the DNAT) and retires any pre-existing `engram-fwd-*` socat
  units.
