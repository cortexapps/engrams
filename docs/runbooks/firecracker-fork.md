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
**local** mmapped file (`MAP_PRIVATE`). The fork adds a small surface
(`git log v1.10.1..engram/live-migration` is the whole delta, ~140 lines) on
top of a pinned upstream `v1.10.x` tag — **inert/opt-in: every existing path
(boot, Full/Diff create, File/Uffd restore) is byte-identical to stock**:

- `src/vmm/src/vstate/memory.rs` — an `msync(MS_SYNC)`-over-the-regions method,
  and thread a `shared: bool` through `from_state` so a file restore can map
  the guest memory `MAP_SHARED | MAP_NORESERVE`. (Upstream's
  `from_raw_regions_file` *already* carries the `shared` flag; `from_state`
  just hardcoded `false` — so this is even smaller than the loopholelabs diff.)
- `src/vmm/src/vmm_config/snapshot.rs` — two `SnapshotType` variants, `Msync`
  (memory-only flush, no state file) and `MsyncAndState`, plus a **top-level**
  `shared` load param (not inside `mem_backend`, which is
  `deny_unknown_fields`).
- `src/vmm/src/persist.rs` — `create_snapshot` / `snapshot_memory_to_file`
  dispatch on the new types (in-place `msync`, no separate file dump) and
  thread `shared` into the File restore path (open the memfile read-write when
  shared). **NB: `snapshot_memory_to_file` lives in `persist.rs`, NOT
  `vstate/vm.rs` — the earlier draft of this runbook was wrong; vm.rs has no
  guest-memory dump.**
- `src/firecracker/src/api_server/...` + `src/firecracker/swagger/firecracker.yaml`
  — surface the new `snapshot_type` values and `shared` through the HTTP API +
  OpenAPI contract.

That `MAP_SHARED` + `msync` is exactly what ADR 0045 **Phase D** (continuous
off-pause flush) and **Phase C** (a consistent, page-readable *source* memory
file for post-copy) require. We host this ourselves rather than depend on
`loopholelabs/firecracker`'s `main-live-migration` branch, which is archived
(2025-09-22), ~211 commits behind its own main, and carries unrelated PVM work.

> **R2 — snapshot wire-format compat is preserved by construction.** Upstream
> serializes machine state as bincode `SnapshotHdr{magic,version}` + CRC64 with
> **no length prefix** (the length-prefix that motivated R2 is a *loopholelabs*
> design we don't copy). Because this surface **never touches
> `src/vmm/src/snapshot/` and never bumps `SNAPSHOT_VERSION`**, stock↔fork
> Full/Diff snapshots restore in both directions — so a mixed fleet during a
> roll is safe and live migration needn't be gated on both-hosts-forked. The
> `build-firecracker` CI job enforces this invariant with a diff-guard.

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

## Standing it up

1. **Create the fork repo.** ✅ Done — `cortexapps/firecracker`, branch
   `engram/live-migration` = the surface as a single auditable commit on the
   pinned upstream `v1.10.1` tag (`git log v1.10.1..engram/live-migration`). The
   delta is the AGPL-review surface; keep it minimal. Built + unit-tested static
   musl on the dev-vm (the binary runs: `firecracker --version` → v1.10.1).
2. **Vendor it as a submodule.** ✅ Done —
   `git submodule add -b engram/live-migration https://github.com/cortexapps/firecracker third_party/firecracker`.
   Trips the `fc_fork` lane on every bump; invisible to the cargo workspace (FC
   declares its own `[workspace]`, so `cargo metadata` / `just check` ignore it).
3. **`build-firecracker` CI job.** ✅ Done — in `bake-images.yml`, gated `if:
   needs.detect.outputs.fc_fork == 'true'`. Builds static musl in
   `working-directory: third_party/firecracker` (FC's pinned `rust-toolchain.toml`
   governs; build only `-p firecracker`), publishes
   `ghcr.io/cortexapps/engrams/firecracker:<fork-sha>` via `oras`, and runs the
   R2 diff-guard. `publish-node-assets` reads the submodule gitlink SHA,
   `oras pull`s that artifact, and passes it as `ENGRAM_FC_SRC`.
   - **Build-env gotchas (baked into the job):** FC pulls `aws-lc-sys` (needs
     `cmake`) + `userfaultfd-sys` (needs `<linux/*.h>` + bindgen/clang). Do NOT
     reuse `publish-host-binaries`'s global `-I/usr/include` `CFLAGS` — it leaks
     glibc headers into `aws-lc-sys`'s musl build. Instead symlink only the
     libc-agnostic kernel uapi dirs (`linux/`, `asm/`, `asm-generic/`) into
     musl's sysroot, and keep `BINDGEN_EXTRA_CLANG_ARGS` for bindgen.
4. **Activate the automated-tracking cron (Phase B M4).** `rebase-fc-fork.yml`
   is the self-maintaining loop: daily it picks the newest **stable** upstream
   release (`vX.Y.Z`, semver-max across **all** majors by default), rebases the
   patch branch onto it, **build-verifies** (static musl) *before* pushing — so
   a broken rebase never clobbers the good branch — then opens an engrams
   submodule-bump PR labeled `fc-fork-bump`, approves it (different identity),
   and enables auto-merge. `auto-enqueue.yml` enqueues it on green (the rulesets
   workaround, reusing the `engrams-automerge` App). Every green bump
   auto-merges, majors included. A rebase **conflict** or a clean-rebase **build
   break** is loud: a tracking issue + a red run (README badge).
   - **To go live, set just two repo vars** — `FC_FORK_REPO`
     (`cortexapps/firecracker`) + `FC_FORK_BRANCH` (`engram/live-migration`).
     **No new secret:** the fork checkout + push reuse the existing cross-org
     `GH_TOKEN` (the same PAT build-firecracker's submodule checkout uses — it
     reads the INTERNAL fork and pushes the rebased branch), and the engrams bump
     PR reuses the `AUTOMERGE_APP_*` App. `FC_UPSTREAM_BASE` is **optional** —
     leave it unset to track all majors; set it to a tag prefix to pin a line
     (`v1.` — trailing dot — stays on v1.x and won't auto-jump to v2.0). (Once
     the fork goes public the read is tokenless, but the push still needs auth.)
   - **First activation is a catch-up jump.** The fork sits on `v1.10.1` (prod
     parity); the newest stable is many releases ahead. The first cron run will
     try to rebase across that whole gap — likely a conflict or build break that
     files the tracking issue. That's expected: do the catch-up rebase by hand
     once (port the surface to the current API, confirm the build), push, and
     from then on the cron handles each incremental release automatically. (Bump
     `FC_VER` in `node-assets-fetch.sh` is no longer needed — the fork IS the
     binary now.)
5. **Risk R2 — snapshot wire-format compat.** ✅ Preserved by construction (see
   the note above): the fork never touches `src/vmm/src/snapshot/` and never
   bumps `SNAPSHOT_VERSION`, so stock↔fork Full/Diff snapshots restore both
   directions — mixed fleets during a `nodeAssetsImage` roll are safe, no
   both-hosts-forked gate needed. The `build-firecracker` diff-guard enforces it;
   the `stock_fork_snapshot_compat` test (Phase B M3) proves it end-to-end.

## The go/no-go spike (S-C1, on the dev-vm)

Before committing to Phase C, prove the smallest end-to-end slice on two
`engram-dev` KVM hosts: forked FC both sides, the source serving memory from a
*frozen* snapshot (skip Phase D), the destination resuming immediately and
demand-faulting exactly one page across the host↔host P2P link — then prove the
crash arm both ways (durable → GCS installs the page; not-durable → clean
rung-1 rewind). That single fault + crash-fallback is the whole gate; the rest
is scale-up.
