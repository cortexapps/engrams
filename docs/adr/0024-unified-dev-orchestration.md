# ADR 0024: One dev orchestrator, one switch-free interface

Status: 2026-05-30 — **Proposed.** The local-dev story has drifted into two
parallel orchestrators that express the *same* prod-shape service map, and the
backend/arch switch has leaked all the way up into the operator interface
(`justfile` + `Tiltfile`). This ADR collapses both onto a single switch-free
interface — `just dev` everywhere — and pushes the host-capability decision down
into one orchestration-layer probe. Nothing here touches the substrate's latency
work (ADRs 0019–0022) or the product plane (ADR 0023); it is purely about how a
developer spins the stack up.

## Context

There are two ways to run the full stack locally, and they share almost no code:

- **macOS** runs `just dev` → `tilt up`. The `Tiltfile` selects the VZ backend
  by `uname`, and prod-shape split mode is an opt-in env flag
  (`ENGRAM_DEV_SPLIT=1`).
- **The Linux KVM dev-vm** runs `just integration-up` → a ~500-line
  `deploy/dev/integration-up.sh` (plus `integration-down.sh` /
  `integration-reset.sh`). It exists because the dev-vm has no Tilt and because
  the Linux host-agent needs `sudo` to create per-VM TAPs (CAP_NET_ADMIN). CI's
  `test-e2e-stack` lane drives the same script.

Both express the identical topology — postgres + registry + fake-gcs + jaeger,
coordinator, host-agent, web — yet one is Starlark under Tilt and the other is
hand-rolled bash with nohup/pidfiles. They drift independently: the recent
macOS `seed-buckets` breakage (fake-gcs-server `network_mode: host`, fixed in
`cb808a9`) was a symptom — a change made for the Linux path silently broke the
mac path because nothing tied them together.

On top of the orchestrator split, the **arch switch leaks up to the operator
interface**:

- `Tiltfile` has a `uname` ladder that picks `vz` vs `firecracker` and
  hard-fails on anything else.
- The justfile has parallel arch-specific recipes: `dev-vz` / `dev-firecracker`,
  `fc-bake-demo` / `vz-bake-demo`, `vz-pull-kernel` vs the FC fetch script.

A developer therefore has to know their host's backend to pick the right recipe.
That is exactly backwards: the tooling should detect the host and Do The Right
Thing.

## Decision

**One interface, switch-free:**

```
just dev          # full prod-shape stack; backend + topology auto-selected
just dev-down     # tear it down
just bake-demo    # build + bake the demo-claude image → local OCI registry
just pull-kernel  # fetch the right kernel for this host
```

`just dev` runs `tilt up` on **every** host. Tilt becomes the single
orchestrator; the `integration-*` scripts are retired and CI's e2e lane runs
`tilt ci`.

**Detection belongs ABOVE the binary, not inside it.** The sandbox backends are
compile-time gated — `VzBackend` is `#[cfg(target_os = "macos")]`, Firecracker
only compiles on Linux — so a single binary physically cannot contain all three.
"Auto-detect among all backends" cannot live in the binary; it is a
host-capability decision, not a runtime branch. The binaries keep their existing
**explicit** selector (`ENGRAM_SANDBOX_BACKEND=firecracker|vz|process`) and do
not change. Production is unaffected: Helm sets the backend explicitly and never
runs the probe.

Detection lives in one place — `deploy/dev/detect-backend.sh` — using a single
rule:

```
/dev/kvm present & readable   -> firecracker
else macOS && arm64           -> vz
else                          -> process
```

The `Tiltfile` calls that probe once and uses its result for **both** coupled
decisions:

1. The concrete `ENGRAM_SANDBOX_BACKEND` passed to the stack (+ the matching
   kernel path).
2. The process topology: `process` → coordinator `mode=all` only (no separate
   host-agent, which has no Process backend); otherwise the **split** topology
   (coord `mode=coordinator` + a `engram-host-agent` process) — making prod-shape
   split the default and auto-degrading on a virt-less host.

The bake/kernel helper scripts derive the guest arch from the same probe, so the
justfile recipe bodies carry no `uname`.

**arm64 built-in harness.** `just bake-demo` bakes `deploy/demo-claude/`, whose
`[harness] builtin = "claude"` makes the baker inject a published harness
artifact. That artifact is currently x86_64-only (`Platform` enum has one
variant), so the baked-harness image can't run on an arm64 VZ guest. We add
`Platform::LinuxArm64`, select the platform from the build request (defaulting to
host arch) instead of hardcoding it, and publish the arm64 harness tag in CI — so
the same `just bake-demo` works on macOS/VZ and the Linux dev-vm. For local dev,
`bake-demo` builds the harness from source and publishes it to the local registry
(catalog env override), rather than pulling the released GHCR tag.

## Phasing

0. **This ADR** (Proposed → Accepted at the end).
1. `deploy/dev/detect-backend.sh` — the single host-capability source of truth.
2. arm64 built-in harness: `Platform::LinuxArm64`, build-request platform
   selection, `engram-cli image build --harness-platform`, CI publishes both
   arch tags.
3. Tilt as the one orchestrator: `tilt` into the flake devShell; `Tiltfile`
   collapses the `uname` ladder to one `detect-backend.sh` probe → concrete
   backend + topology, with the Linux host-agent run under `sudo -n
   --preserve-env` and a prebuilt-binary `serve_cmd` branch (for CI). justfile
   collapses to `dev` / `dev-down` / `bake-demo` / `pull-kernel`; arch recipes
   retired.
4. Retire `integration-{up,down,reset}.sh`; port CI `test-e2e-stack` to
   `ENGRAM_INTEG_BIN_DIR=… tilt ci` (consume prebuilt binaries, no cargo build).
5. dev-vm skill (forward Tilt UI :10350, NOPASSWD sudoers for the host-agent,
   tmux note) + fold the per-backend demo docs into the one `just dev` story.

## Consequences

- **Wins:** one orchestrator, one interface, ~500 fewer lines of bespoke bash; a
  change to the dev stack can no longer break one host while leaving the other
  green, because there is only one description of it. The operator never picks a
  backend.
- **Costs / risks:**
  - The CI `tilt ci` port (Phase 4) is the riskiest leg — the lane is tuned
    around prebuilt-binary layout and artifact-prune ordering. Mitigated by the
    Phase-3 `ENGRAM_INTEG_BIN_DIR` branch (no cargo build in CI) and validating
    on the branch before deleting `integration-up.sh`.
  - The Linux host-agent under Tilt needs NOPASSWD sudo on the dev-vm; macOS
    involves no sudo.
  - The detection rule now exists only in shell. That is acceptable because prod
    never runs it; a drift would only affect which dev backend is chosen, and the
    binary still validates the concrete choice it's given.

## Alternatives considered

- **Auto mode inside the binary (`ENGRAM_SANDBOX_BACKEND=auto`).** Rejected: the
  backends are `cfg(target_os)`-gated, so a binary can't contain all variants —
  "auto" would imply every binary has everything, which is false. Detection is a
  host-capability concern that belongs in the orchestrator.
- **Keep two orchestrators behind one `just dev` dispatcher.** Rejected: it
  preserves the duplication (and the drift) this ADR exists to remove.
- **Drop Tilt, make `just dev` a portable script everywhere.** Rejected: loses
  Tilt's UI / log aggregation / readiness model that the inner loop relies on;
  the script path is the thing we're retiring, not the engine.
