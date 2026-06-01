# ADR 0025: Own the Firecracker guest kernel

Status: 2026-06-01 — **Accepted.** Shipped + prod-validated: the custom kernel
(`vmlinux-engram-6.1.102-1`) is live on the FC-host MIG, and a prod `dev-engrams`
session boots it (`uname -r` = 6.1.102) and runs **Docker 29.5.2 + `docker
compose up postgres` with zero workarounds** (`pg_isready` → accepting
connections on the published port). Dev-vm-verified first, then rolled through
the auto-pipeline (base bake installed the kernel via the token'd API → thin →
tf-apply → MIG roll). _Original proposal below._

Engrams has booted FC sessions on
Firecracker's stock CI guest kernel (`vmlinux-5.10.223`, pulled from the public
`spec.ccfc.min` bucket). That kernel is deliberately minimal and **cannot run
Docker inside a sandbox** — which blocks the "engrams on engrams" dogfood loop
(ADR 0023), where the in-session agent wants the real `just dev` (Tilt + `docker
compose` infra: Postgres / registry / fake-gcs / jaeger). This ADR moves engrams
to a **self-built guest kernel** = the stock Firecracker microvm config + a small
netfilter fragment, published as a GitHub release asset.

## Context

`just dev` inside a session already degrades to the ProcessBackend (no nested
KVM needed — `detect-backend.sh`, ADR 0024), and Docker's *runtime* (overlay2,
cgroup v2, runc) works fine in the FC guest. The blocker is purely the **stock
kernel's stripped netfilter stack**. Verified on the dev-vm by booting throwaway
Docker images as real FC sessions and exercising dockerd in-guest:

- `vmlinux-5.10.223` has **no `nf_tables`, no `raw` table, no IPv6 NAT**. So
  bookworm's default `iptables-nft` can't initialise, and Docker 28+ inserts a
  `raw`-table DROP rule per container endpoint that fails — no bridge container
  starts. Working around it (force `iptables-legacy`, pin Docker 27.5.1,
  `userland-proxy:false`, `ip6tables:false`) is a fragile pile and still left a
  ~40s per-container latency.
- **Every published Firecracker-CI binary is stripped the same way** — extracted
  the real configs of `5.10.223`, `6.1.102`, `6.1.128`, `6.1.141`: none have
  `nf_tables`/`raw`. (The firecracker repo's `main` config *does*, but no
  published binary was built from it.) Kata's container kernel has `raw` but no
  `nf_tables`.

Studied E2B (`e2b-dev/infra`): they don't fight the minimal kernel — they boot a
**fuller, custom-built** guest kernel (`vmlinux-6.1.158`) and let users install
Docker themselves. Same conclusion for engrams.

## Decision

Build the FC guest kernel ourselves:

- **Config** = a **vendored** copy of Firecracker's "full" microvm config
  (`deploy/kernel/microvm-kernel-ci-x86_64-6.1.config`, from firecracker SHA
  `8a7e8a01`; FC-bootable: `virtio-mmio` + vsock + `ip_pnp`) **+ `deploy/kernel/
  engram-docker.fragment`** enabling `nf_tables` (+compat/nat/masq/redir/ct/fib),
  the legacy `raw`/IPv6 tables, bridge-netfilter, and VXLAN. Merge via
  `scripts/kconfig/merge_config.sh` + `make olddefconfig`;
  `deploy/kernel/build-fc-kernel.sh` asserts every required symbol (Docker's
  needs *and* the FC-boot options, incl. `FUSE_FS`) survived before building.
- **Why vendor the base (not pin a Firecracker release tag):** the choice is
  boot-critical. Firecracker's *release* configs (e.g. `v1.10.1`) are far more
  stripped than their `main` "full" config — among other things they drop
  `CONFIG_FUSE_FS`. dev-vm-verified: a kernel built on the v1.10.1 base *starts*
  the FC VM but **agentd never comes up** and base-snapshot capture times out;
  the `main`/vendored base boots agentd fine. Vendoring pins the exact known-good
  config and removes any dependency on which upstream ref we happen to fetch.
- **Reproducible pins:** vendored base config + in-repo fragment + linux source
  version + a `KERNEL_REV` bumped on base/fragment changes. Asset
  `vmlinux-engram-<ver>-<rev>`.
- **Publish** as a GitHub release asset on `cortexapps/engrams` via
  `.github/workflows/build-fc-kernel.yml` (manual dispatch — kernel changes are
  rare and prod-wide). The repo is private, so consumers download with a token.
- **Consume:** `install-fc-kernel.sh` (prod FC-host bake) and
  `fetch-fc-test-artifacts.sh` (dev/test) fetch the asset to the unchanged
  install path `/usr/local/lib/engram/vmlinux` (so the TF `kernel_image_path`
  default is untouched).

Result, dev-vm-verified end to end (build → boot FC session → snapshot/restore):
**plain Docker 29 + compose, default `nft` backend, default `userland-proxy`,
zero daemon workarounds, no per-container latency.**

## Consequences

- The guest kernel is shared by every FC session. Rolling it is a **prod-wide
  event**: the FC-host MIG re-bakes (new `install-fc-kernel.sh`) and **all
  ADR-0020 base snapshots must be re-captured** (snapshots are kernel-specific) —
  i.e. re-enable every image after the host roll. Coordinate per
  `reference_engrams_deploy_auto_triggers` (coord rolls first → broken window
  until host re-bake + re-enable).
- We now own a kernel build (security updates, version bumps) — but it's a thin
  layer over the stock FC config, rebuilt by one manual workflow.
- Boot/restore/clock-steering must be re-validated on the new kernel (done on the
  dev-vm for boot + restore; prod validation at roll time).
- The `dev-engrams` image drops all its Docker workarounds and runs plain Docker
  29 (separate change in engrams-internal).

## Commit chain

- OSS `b52d821` — build capability (this ADR Proposed + `deploy/kernel/` +
  `build-fc-kernel.yml`); pushed, then `build-fc-kernel.yml` dispatched to
  publish the asset.
- OSS `c5fa6be` — consumers + auth (`install-fc-kernel.sh`,
  `fetch-fc-test-artifacts.sh`, `ci.yml` token, packer `gh_token`, pidfd doc,
  integration-session).
- engrams-internal `ccecc37` — base bake passes `gh_token` to packer.
- engrams-internal `2fb1b4d` — dev-engrams plain Docker 29.
- _(this commit)_ — flip to Accepted.

**Roll notes.** Pushing both lanes (host_base + host_binaries) at once made the
deploy pipeline fan out base+thin in parallel + double-bake (redundant, both on
the custom kernel) — a one-line sequencing improvement for a follow-up. The
GHCR pull of the 7.6 GB dev-engrams image to a cold-cache fresh host hit 429s;
it converged on retry (chunk cache accumulates). **Follow-up housekeeping:** the
other previously-enabled images (demo-claude, older dev-engrams tags) still carry
old-kernel base snapshots and must be re-enabled (delete + re-POST) to
re-capture on 6.1.x before they can boot again.
