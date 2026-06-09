# Runbook: the engrams Firecracker fork (ADR 0045 Phase B)

ADR 0045's golden state (post-copy live migration + `MAP_SHARED` off-pause
flush) needs a small fork of Firecracker. This runbook is the operational
companion to ADR 0045 Phase B: how the fork is vendored, kept rebased onto
upstream, built, and consumed — and the steps to **stand it up** (the parts
that need a fork repo + the dev-vm, which can't be done from the engrams repo
alone).

## The fork, in one paragraph

Upstream Firecracker has no live migration; its only cross-host primitive is
snapshot-on-source → restore-on-destination, and its UFFD restore reads a
**local** mmapped file (`MAP_PRIVATE`). The fork adds a **~4-file surface** on
top of a pinned upstream `v1.10.x` tag:

- `src/vmm/src/vstate/memory.rs` — a `shared: bool` that flips the guest-RAM
  mmap to `MAP_SHARED | MAP_NORESERVE`, and an `msync(MS_SYNC)` over the
  regions.
- `src/vmm/src/vmm_config/snapshot.rs` — two `SnapshotType` variants, `Msync`
  and `MsyncAndState`, plus `shared` on the load params.
- `src/vmm/src/persist.rs` — `create_snapshot`/restore dispatch on the new
  types (memory-only flush; thread `shared` through).
- `src/vmm/src/vstate/vm.rs` — branch `snapshot_memory_to_file` by type.

That `MAP_SHARED` + `msync` is exactly what ADR 0045 **Phase D** (continuous
off-pause flush) and **Phase C** (a consistent, page-readable *source* memory
file for post-copy) require. We host this ourselves rather than depend on
`loopholelabs/firecracker`'s `main-live-migration` branch, which is archived
(2025-09-22), ~211 commits behind its own main, and carries unrelated PVM work.

> **AGPL — read before copying a line.** silo/Drafter and the loopholelabs FC
> branch are AGPL. Treat them as a *design reference to reimplement clean-room*
> from the public diff's mechanism, never as code to vendor. The ~4-file
> surface is small enough to reimplement against pristine upstream.

## What's already wired in engrams (this PR)

- **Consume seam** — `docker/node-assets-fetch.sh` honors `ENGRAM_FC_SRC`: when
  set, it stages that pre-built binary as `$OUT/firecracker` instead of
  downloading the upstream release. The node-assets image contract is
  unchanged (still a single `/opt/engram/firecracker`), so the host-agent +
  the K2 reattach story are untouched.
- **Change detection** — `.github/scripts/detect-rebake-lanes.py` has an
  `fc_fork` lane on `third_party/firecracker` + `.gitmodules`, folded into the
  `images` lane. So bumping the submodule pointer rebuilds node-assets and the
  operator drain-gated-rolls the fleet, exactly like any host-binary change.
  Inert today (those paths don't exist yet).
- **Daily rebase** — `.github/workflows/rebase-fc-fork.yml` rebases the fork's
  patch branch onto the newest upstream tag every day; on conflict it
  opens/updates a tracking issue and goes red (README badge). **Inert until
  `FC_FORK_REPO` is set** (the gate skips cleanly).

## Standing it up (needs a fork repo + the dev-vm)

1. **Create the fork repo.** `cortexapps/firecracker`, branch
   `engram/live-migration` = the ~4-file patch series as a small commit series
   on the pinned upstream `v1.10.x` tag. Keep the delta minimal + auditable
   (it *is* the AGPL-review surface).
2. **Vendor it as a submodule.** In engrams:
   `git submodule add -b engram/live-migration https://github.com/cortexapps/firecracker third_party/firecracker`.
   This trips the `fc_fork` lane on every bump.
3. **Add the `build-firecracker` CI job** to `bake-images.yml`, modeled on
   `publish-host-binaries` (musl-static build + an `oras` OCI artifact to
   `ghcr.io/cortexapps/engrams/firecracker:<sha>`), gated `if:
   needs.detect.outputs.fc_fork == 'true'`. Have `publish-node-assets` pull
   that artifact and pass it as `ENGRAM_FC_SRC` to `node-assets-fetch.sh`.
4. **Enable the rebase cron.** Set the repo variables/secrets the workflow
   gates on: `FC_FORK_REPO` (`cortexapps/firecracker`), optional
   `FC_FORK_BRANCH` (default `engram/live-migration`) + `FC_UPSTREAM_BASE`
   (default `v1.10`), and `FC_FORK_TOKEN` (push access to the fork). The badge
   goes live.
5. **Risk R2 — snapshot wire-format skew.** The fork length-prefixes the state
   buffer, so stock↔fork restore may be incompatible. Add a CI compat test and
   gate live migration on both-hosts-forked if it is. Mixed fleets are
   guaranteed transiently during a `nodeAssetsImage` digest roll.

## The go/no-go spike (S-C1, on the dev-vm)

Before committing to Phase C, prove the smallest end-to-end slice on two
`engram-dev` KVM hosts: forked FC both sides, the source serving memory from a
*frozen* snapshot (skip Phase D), the destination resuming immediately and
demand-faulting exactly one page across the host↔host P2P link — then prove the
crash arm both ways (durable → GCS installs the page; not-durable → clean
rung-1 rewind). That single fault + crash-fallback is the whole gate; the rest
is scale-up.
