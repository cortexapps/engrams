# ADR 0045: The unified memory substrate — one guest-memory architecture for density, lazy restore, live teleport, and fast teardown

Status: 2026-06-09 — **Proposed → partially Accepted** (rewritten around the end-state substrate; see "What changed in this rewrite"). Landed: Phase A (retire reactive evac, #136), Phase F's teleport test surface (snapshot-rehome form, #137), Phase E1's scale-down decision engine (#138), and **Phase B — the fork itself**: ported onto upstream FC, vendored, drop-in-proven, self-maintaining via the tracking cron, caught up to **v1.16.0**, and **rolled to prod** (#139/#149/#151–#153/#158/#159). What remains is **Phase D — the unified memory substrate** (gated on the S0 feasibility spikes) and everything that rides it: Phase C (post-copy live teleport on the substrate), the E1 actuator + E2, and Phase F's live path. Flips fully to **Accepted** phase-by-phase as the commit chain lands. Supersedes ADR 0043 Phase 3b (retire evac) and carries forward ADR 0042's Tier-3 "differentiated move."

## What changed in this rewrite (2026-06-09)

The original ADR framed the golden state as two loosely-coupled fork features: `MAP_SHARED` off-pause flush (old Phase D) and post-copy migration (Phase C). Design review against the running system found the old Phase D **does not compose with production**, and that chasing it piecemeal would have either regressed density or forked the memory path per lifecycle op:

- **The old Phase D's mechanism was unimplementable as written.** `Msync` flushes file-backed `MAP_SHARED` guest memory — but no prod session has that. UFFD restores build guest RAM from *anonymous* pages (`UFFDIO_COPY` installs private copies); File restores map the *immutable base* memfile `MAP_PRIVATE`, so guest writes COW to anonymous pages msync never sees. The old coexistence claim ("different inodes, opposite directions — no interaction") hid a missing object: a live per-sandbox memfile that nothing creates. Creating one naively either lands a multi-GB materialization on the create/resume latency path or — by giving each sandbox a private file — silently retires the page-cache density win that ADR 0022/0039 shipped to prod.
- **Resume has a hidden density loss today.** Fresh creates share clean base pages across same-template siblings (`MAP_PRIVATE` of one resident memfile: 3 siblings measured Σpss/Σrss = 35%). Idle **resumes don't**: every UFFD-installed page is a private copy, even the large majority still byte-identical to the base. The original ADR never named this.
- **The kernel primitive matrix pins the end-state design** (details under Decision): UFFD `MINOR`/`WP` modes require shmem; file-backed (ext4) mappings support neither; KSM is anonymous-only; msync-to-disk requires a regular file. There is exactly one construct that yields shared-clean + private-dirty + lazy-from-anywhere + externally-readable in a single mapping, and it is the substrate this ADR now commits to.

So the rewrite replaces "two fork features" with **one memory architecture** that cold boot, idle resume, teardown, and teleport all run on — and re-derives Phases C/D from it. Phases A/B/E/F and all shipped history are unchanged. Interim shortcuts found during the review (a `process_vm_readv` teleport source that needs no fork; a teardown pipeline reorder on the existing diff-chain) are recorded as de-risk fallbacks, explicitly not the plan of record.

## Context

The substrate has evolved across three ADRs:
- **ADR 0042** (survey) named the differentiated move: **post-copy live migration with a durable-snapshot fallback** — "teleport" a running session host-to-host with minimal downtime, crash-safe because the working set is already durable in GCS. That intersection is unoccupied: silo/Drafter do post-copy *without* a durable fallback; everyone else (E2B, CodeSandbox, Modal, Replit) does snapshot-and-rehome *without* post-copy. Our durable chunk store gives us both.
- **ADR 0043** (roadmap) shipped the no-fork wins — P0 fail-fast, P1 immediate-resume / non-blocking UFFD prefetch, P2a relaxed checkpoint cadence — and explicitly gated the fork.
- **ADR 0044** (shipped) moved the Firecracker host fleet to Kubernetes. Two consequences land here: (1) the **drain-driven evacuation machinery is now load-bearing** — K3 drain = mark `Evacuating` → the `evac_resumer` scanner → resume on a peer — so ADR 0043's "fully retire session evac" can no longer be taken literally; and (2) ADR 0044 K4 shipped demand autoscaling but **deferred drain-gated scale-*down***.

And the memory paths in production today, which this ADR unifies:
- **Fresh create (cold boot)** = `RestoreMode::File`: `MAP_PRIVATE` of the resident per-template base memfile (ADR 0022 Option A, promoted to prod default by ADR 0039). ~60 ms restore, no handler, and density — clean base pages shared across same-template siblings through the page cache.
- **Idle resume** = `RestoreMode::Uffd`: the `engram-uffd-handler` lazily installs 512 KiB chunks via `UFFDIO_COPY` into anonymous guest RAM, racing a working-set prefault (ADR 0043 P1). Cross-host-capable, but every installed page is private — no sharing, even for base-identical pages.
- **Teardown / checkpoints** = the diff-chain (ADR 0028/0038/0039): periodic `Diff` captures at 38–53 ms guest pause, sparse re-chunk, 32-way parallel GCS upload. Durability is bounded-loss by cadence.

What remains to reach the golden state, and what this ADR commits to:
1. **Retire the *reactive* evac, keep the *drain-driven* evac** (shipped, Phase A). The drain-driven path is the seam Phase C upgrades.
2. **Fork Firecracker** (shipped, Phase B) — now scoped as the enabler of the substrate: stock FC cannot map guest memory from a shared shmem object nor register UFFD beyond `MISSING` mode.
3. **The unified memory substrate** (Phase D, rewritten) — one guest-memory architecture giving density + lazy restore + external readability + enumerable dirty state, replacing the File/Uffd split.
4. **Post-copy live teleport on the substrate** (Phase C, rewritten) — the headline: live teleport, crash-safe, with near-nothing to do at handoff because the substrate already separates base (resident everywhere) from divergence (the overlay).
5. **True autoscale-down** (Phase E) — the payoff that rides the teleport seam: the fleet continuously self-compacts to its cost-optimal node count with minimal perceived disruption.

The unifying insight, sharpened by the rewrite: **the expensive thing in every lifecycle operation — boot, resume, checkpoint, teardown, teleport — is moving or copying guest memory. Make guest memory an explicitly-structured object (shared base + per-sandbox overlay) and every operation becomes a cheap manipulation of that structure** instead of a bulk copy: boot = map the base; resume = map the base + lazily fill the overlay; checkpoint/teardown = upload the overlay; teleport = move only the overlay. We don't build five mechanisms; we build one substrate and five thin consumers.

## Decision

**The substrate.** Every session VM's guest memory — cold boot, idle resume, post-teleport destination — is built the same way:

```
guest RAM = MAP_SHARED of the per-template BASE shm object   (one per host; lazily populated; shared by all
            registered UFFD MINOR | WP                        same-template sessions)
            + per-sandbox OVERLAY shm file                    (starts empty; accumulates exactly the dirty set)
```

- **Read fault on a base-identical page** → the handler resolves it `UFFDIO_CONTINUE` against the shared base object: zero copy, one physical page serves every same-template session on the host. This is File-mode's density **extended to resumes** — strictly better than today, where resume privately copies everything.
- **First write to a page** → `WP` fault → the handler `MAP_FIXED`-carves a 4 KiB per-sandbox overlay page over that guest address, copies the base content, write-unprotects, wakes. Dirty pages are private to the sandbox, and — the structural payoff — **the overlay file both enumerates and contains the dirty set**.
- **Base population is lazy and shared**: on the first `MINOR` miss for a base page on the host, the handler `pwrite`s the 512 KiB base chunk (from the resident base memfile / local chunk cache) into the base shm object, then `CONTINUE`s the faulting page; neighbors hit fast `MINOR`→`CONTINUE`. Hole-punch reclaims cold base regions under pressure.
- **Resume / teleport destination**: the overlay is seeded lazily from the session's chunk manifests on `MINOR` miss (pwrite the chunk into the overlay → `CONTINUE`). Phase C's two-source race (peer host vs GCS) plugs in at exactly this fill point. Base pages never transfer at all — only divergence moves.
- **Teardown / checkpoint**: no FC memory capture. Pause → `state.bin` + quiesce; chunking + GCS upload read the overlay file off the critical path — valid even after the FC process is gone. The KVM dirty log remains a cross-check, not the export mechanism.
- **Teleport source**: the base already exists on every host; the destination needs only the overlay (lazily, raced against GCS) plus `process_vm_readv` for any in-flight pages. Near-nothing happens at handoff.

**One path, retired split.** `RestoreMode::{File,Uffd}`, `effective_restore_mode(fresh)`, and `ENGRAM_FC_BASE_RESTORE_MODE` retire once the substrate passes its parity gates (Phase D3/D4) — a clean break, not a third mode living alongside two old ones.

**Why this construct and not something simpler — the kernel primitive matrix.** The requirements are: (a) clean base pages physically shared across sessions (density); (b) dirty pages private; (c) pages fillable lazily from arbitrary sources (chunks, peer host, GCS); (d) current memory readable from outside the VMM (teleport source, post-mortem upload); (e) the dirty set enumerable without a stop-the-world capture. Against the kernel's actual capabilities:
- File-backed (ext4) `MAP_PRIVATE` gives (a)+(b) — today's File mode — but supports **no** UFFD interception, so (c) is impossible: it only works when the complete image already sits on local disk. That is precisely why resume can't use it.
- Anonymous + UFFD `MISSING` gives (c) — today's Uffd mode — but every installed page is private: no (a), and (e) requires KVM-dirty-log captures.
- `UFFDIO_MINOR`/`UFFDIO_CONTINUE` and UFFD-WP exist **only on shmem/hugetlbfs** (MINOR ≥5.13; WP-on-shmem ≥5.19). KSM is anonymous-only. msync-to-disk needs a regular file (tmpfs has no writeback) — which is why the old Phase D's `Msync` design was unimplementable against any UFFD-capable backing.
- Therefore: shared **shmem** base (gives (a) via `CONTINUE`, (c) via `MINOR`, (d) via the file), WP + overlay-carve (gives (b), (e)). No other composition of available primitives satisfies all five. This is also — derived clean-room from primitives, never from the AGPL implementations — the architectural neighborhood the strongest sandbox substrates converge on.

**Honest bounds.**
- *Downtime*: post-copy handoff = pause + `state.bin` move + destination resume; the destination's bring-up (FC spawn, netns, load) dominates the first iteration and is amortized by **pre-staging** (Phase C). Budget claims are deferred to measurement (S0.4 + Phase C kill-tests) — the old ~7–12 ms figure was QEMU's, not ours.
- *Crash-safety is bounded, not transactional*: recovery is from the last durable flush; loss ≤ one checkpoint interval. The periodic checkpoint (ADR 0043 P2a) stays the always-on backstop; it is *the* thing that makes our post-copy crash-safe where the field's is not.
- *Density is a hard gate, not a hope*: Phase D3/D4 may not ship unless measured density on real images is ≥ today's File mode and restore latency ≤ today + ε.
- *New standing costs*: VMA carving (~2 VMAs per first-written page → raise `vm.max_map_count`; ~µs `mmap` on each first write; ~20 MB kernel VMA overhead at 50k dirty pages), shmem pages are swappable but not page-cache-reclaimable (RAM accounting differs from today's reclaimable memfile cache), and kernel floors (WP-on-shmem ≥5.19: GKE COS ≥6.1 ✓; the dev VM needs a kernel bump).

**Key sub-decisions:**

1. **Reconcile "retire evac" rather than delete it wholesale** (shipped, Phase A). The `Evacuating` state + `evac_resumer` scanner + `evacuate_dead_source` are the K3-drain engine; only the *reactive* producers were retired.

2. **Fork Firecracker, minimal surface, ported onto upstream** (shipped, Phase B). The fork's v2 surface (Phase D1) grows to: guest memory constructed from a configured shared shm object + per-sandbox overlay, and UFFD registration in `MINOR|WP` (not just `MISSING`). The v1 `Msync`/`MsyncAndState`/`shared` surface is superseded by the substrate (tmpfs has no writeback to msync) and will be repurposed or retired honestly in D1. The invariants stand: never touch `src/vmm/src/snapshot/`, never bump `SNAPSHOT_VERSION` (the R2 byte-compat guard), keep the delta a small auditable commit series, stay clean-room.

3. **Reuse the fill seam.** The substrate keeps the `engram-uffd-handler` as the single fault authority; Phase C's migration mode is a producer swap at the overlay-fill point (race `{p2p.need_at, store.get_chunk}`, first SHA-256-verified wins), exactly as the original ADR planned at `chunked.rs`' `cache.get(hash, || store.get_chunk(hash))` — the surrounding singleflight / idempotency / hash-verify machinery is reused, not rebuilt.

4. **Crash-safety is the tested default, not a catch block.** GCS is a co-equal, always-armed parallel arm of the overlay fill; source death → rung-1 rewind via the existing coherent-checkpoint recovery. Kill-tests at every migration phase gate Phase C.

5. **Go directly to the end-state; record the shortcuts as fallbacks.** The review found two cheaper interim paths (below, "Interim fallbacks"). We are explicitly **not** taking them as the roadmap — the substrate is the point of carrying the fork — but they are documented and available if the S0 spikes kill the design.

## Phases

| Phase | What | Fork? | Status |
|---|---|---|---|
| **A** | Retire *reactive* evac (dead-host auto-evac + NBD-loss); keep drain-driven evac; lazy-resume-on-next-access | no | ✅ **shipped** — #136 |
| **B** | Fork Firecracker — vendored + auto-rebased + detect-changes-wired; v1 surface (`MAP_SHARED`+`Msync`) superseded by D1's v2 surface | — | ✅ **done + prod-rolled at v1.16.0** (#139/#149/#151–#153/#158/#159) |
| **D** | **The unified memory substrate** — shared base shm + UFFD `MINOR\|WP` + per-sandbox overlay; one restore path; teardown on the overlay. Gated on the **S0 spikes** | yes (v2) | ⬜ pending — S0 next |
| **C** | Post-copy live teleport **on the substrate** (two-source overlay fill + durable GCS fallback) | yes | ⬜ pending — gated on D3 |
| **E** | True autoscale-down — E1 (no-fork, idle-only) now; E2 (aggressive) after C | E2 only | ◐ **E1 decision engine shipped** (#138, log-only); E1 actuator + E2 pending |
| **F** | Admin + web UX for pause / resume / teleport — the test surface; snapshot-rehome early, live under the same verb at C | no | ◐ **teleport shipped** (#137); pause/resume + live-path swap pending |
|  |  |  |  |

**Dependencies:** `B (✅) → S0 → D1 → D2 → D3 → C`; D4/D5 follow D3 and parallelize with C's build-out. A is independent and shipped. E1 and F's snapshot-rehome form are fork-independent (shipped); E2 and F's live path are config/impl upgrades once C lands. **S0 is the new decision gate**: the spikes either validate the substrate or trigger a documented fallback before any fork-v2 code is written.

## Phase detail

### Phase A — retire reactive evac (no fork)

**Status: ✅ shipped (#136).** Landed exactly as designed below — `dead_host.rs` routes recoverable sessions to `Idle`, `nbd_loss_trigger.rs` + the `nbd_unhealthy` wire field are gone, and the live-PG routing test guards it.

Retire the reactive evac, keep the drain-driven evac. **No DB migration** (`Evacuating` + `host_lost` stay valid `sessions.status` values; nothing is removed). **No new resume code** — routing a dead host's recoverable sessions to `Idle` lands them on the existing `/resume` path (`resume_session` → `resume_from_idle`).

- **`dead_host.rs`** — drop `auto_evac_enabled()` + the `ENGRAM_DEAD_HOST_AUTO_EVAC` flag; the detector's stage-2 routing becomes `snapshot.is_some() || has_live_manifest → Idle` (lazy resume; preserves disk-only recoverability via `resume_disk_only_cold_boot`), else `Dead`. The detector itself stays — sessions must not linger `Active` pointing at a dead host.
- **Delete `nbd_loss_trigger.rs`** + its `pub mod` + the heartbeat hook, and remove the now-always-empty `nbd_unhealthy` wire field across `engram-protocol` / `engram-coordinator` / `engram-host-agent` (incl. the never-wired `NbdHealthMonitor`) as one atomic cross-crate commit. The field is `#[serde(default)]` everywhere, so the roll is order-independent.
- **Keep untouched:** `evac_resumer` (the scanner), `evacuate_dead_source` + helpers (still the scanner's relocation primitive), `admin.rs` drain/evacuate/cordon/uncordon, the `HostLost → Evacuating` transition edge (harmless, unused, available to Phase C).
- **Tests:** a live-PG dead-host routing test (recoverable → `Idle` *not* `Evacuating`; non-recoverable → `Dead`) added to `admin_evac_live_pg.rs` (already in CI's live-PG allowlist); existing `evacuate_dead_source` + drain e2e tests stay green.

This is the bounded-crash-window posture ADR 0042/0043 chose for the *unplanned* case (E2B's explicit-durability stance), accepted because the reactive machinery cost more than the durability it bought.

### Phase B — fork Firecracker

**Status: ✅ DONE — port + pipeline + drop-in proof + self-maintaining cron + prod roll all landed.** #139 landed the maintenance surface (the `node-assets-fetch.sh` `ENGRAM_FC_SRC` consume-seam, the `fc_fork` detect lane, the daily `rebase-fc-fork.yml` cron); #149/#151–#153 the port + vendor + stock↔fork compat test; #158/#159 the catch-up to upstream **v1.16.0**. The fork: `cortexapps/firecracker` branch `engram/live-migration`, a clean-room MAP_SHARED/`Msync` surface (~6 files) on upstream **v1.16.0**, lockstep-tagged `engram-v1.16.0` (`engram-v1.10.1` kept for history). Vendored at `third_party/firecracker`; the `build-firecracker` job (fc_fork-gated, `oras` artifact keyed by fork SHA + the version-agnostic R2 diff-guard) feeds `publish-node-assets` via `ENGRAM_FC_SRC` — green end-to-end on real CI. **Prod-rolled 2026-06-09:** the FC host fleet runs the fork at v1.16.0 (node-assets `5621476`, host-agent #154); all three active images boot + restore fresh v1.16 base snapshots in **~600–780 ms** (smoke-tested in prod), so the fork is a proven drop-in under load, not just in CI.

**Note (this rewrite):** the v1 patch surface (`shared` File-load flag + `Msync`/`MsyncAndState`) was built for the old Phase D and is **superseded by the substrate** — Phase D1 replaces it with the v2 surface (shm-backed guest memory + `MINOR|WP` registration) and repurposes or retires the v1 pieces honestly. Everything else about Phase B — the vendoring, the build pipeline, the R2 guard, the self-maintaining rebase cron, the prod roll — carries forward unchanged and is exactly the infrastructure D1 needs.

**Drop-in proof (M3, dev-vm engram-dev, real KVM):** the forked binary is a verified drop-in for stock FC — the existing `#[ignore]`'d FC suite (`boot`, `snapshot` = Full+File restore, `snapshot_uffd` = Full+UFFD restore ×2, `cross_host_restore`, `lifecycle`) is **green against the fork**, and the new `stock_fork_snapshot_compat` test is **green both directions** (stock-made snapshot restores on the fork *and* fork-made restores on stock — the R2 byte-compat proof). So the fork boots + snapshots + restores cleanly and is interoperable with stock; the irreversible decision is committed and safe. (The compat test is wired into the `test-firecracker` CI job, staging both binaries.)

**Self-maintaining (M4):** `rebase-fc-fork.yml` rewritten into the full track→rebase→build-verify→bump→auto-merge loop — daily it picks the newest stable upstream release (semver-max across all majors by default; `FC_UPSTREAM_BASE` optionally pins a line), rebases the patch branch, **build-verifies before pushing** (a broken rebase never clobbers the good branch), then opens an `fc-fork-bump` engrams PR (via the `engrams-automerge` App, approved from a different identity, auto-merge enabled) that `auto-enqueue.yml` (the generalized ex-`dependabot-auto-enqueue`) enqueues on green. Every green bump auto-merges, majors included; a rebase conflict or clean-rebase build break is loud (tracking issue + red badge). Activated (repo vars set; reuses the cross-org `GH_TOKEN`, no new secret). The fork is **lockstep-tagged** `engram-v<upstream>` (the base-finder uses `--match 'v[0-9]*'` to ignore those tags), so "what FC are we on" is one `git describe`. The first run correctly flagged the `v1.10.1`→current gap; the **catch-up to v1.16.0 is done** (re-port onto v1.16.0's refactored memory backend — `snapshot_memory_to_file` is now in `vstate/vm.rs`, `shared` rides `snapshot_file`, `SNAPSHOT_VERSION` is `10.0.0` and the R2 guard is now version-agnostic; `libseccomp-dev` added to the FC build deps), dev-vm-validated (static drop-in + compat green both ways on v1.16.0). From here the cron handles each release incrementally.

**The v1.10→v1.16 prod roll (the snapshot-format cutover).** FC's snapshot format is **not** cross-version compatible — `SNAPSHOT_VERSION` went `4.0.0` (v1.10.1) → `10.0.0` (v1.16.0), and a 4.0.0 snapshot can't restore on a 10.0.0 binary. So R2's "byte-compatible, mixed-fleet-safe" guarantee is **same-version only**: a roll that crosses a `SNAPSHOT_VERSION` change is a coordinated cutover (drain + base-snapshot re-capture), not a gradual mixed roll. Done in an all-idle/0-user window: bump the HostFleet CR `nodeAssetsImage` → operator drain-roll; mark stale idle sessions dead (their v1.10.1 snapshots can't resume); re-capture each active image's base on v1.16 via `POST /api/v1/enabled-images/refresh`. (Two operational footguns worth recording: an all-digit node-assets SHA tag YAML-coerces to a float in helm values — quote it; and cancelling a `helm upgrade` mid-flight orphans the release in `pending-upgrade` — `helm rollback` clears it.) This surfaced **[#160](https://github.com/cortexapps/engrams/issues/160)**: the coord's base-reuse logic matches on manifest digest + chunk presence but **not** the capturing FC version, so it can reuse a base that's incompatible with the rolled FC (worked around by re-baking the image to change its digest). The M4 cron auto-tracking majors means a `SNAPSHOT_VERSION`-changing release will recur, so #160 should land before the next one (a per-bump SNAPSHOT_VERSION gate on the cron was considered and deferred — handle by watching bump PRs for now). The headline CVEs of the v1.10→v1.16 span (virtio-PCI CVE-2026-5747, jailer CVE-2026-1386) don't affect us — we use neither `--enable-pci` nor the jailer (ADR 0044 K2).

**Implementation divergences (M1/M2):**
- **The surface is `persist.rs`, not `vstate/vm.rs`.** `snapshot_memory_to_file` lives in `persist.rs`; `vm.rs` has no guest-memory dump. The real ~5-file surface: `vstate/memory.rs` (msync + thread `shared` through `from_state`) + `vmm_config/snapshot.rs` (the two variants + top-level `shared` load param) + `persist.rs` (dispatch) + `api_server/{request/snapshot.rs,mod.rs}` + `swagger/firecracker.yaml`. Upstream's `from_raw_regions_file` *already* carries the `shared` MAP_SHARED flag (only `from_state` hardcoded `false`), so the diff is ~140 lines.
- **R2 (snapshot wire-format skew) is a non-issue by construction, not a risk to mitigate.** Upstream serializes state as bincode `SnapshotHdr{magic,version}` + CRC64 with no length prefix (that was the *loopholelabs* design we don't copy). Holding the invariant "never touch `src/vmm/src/snapshot/`, never bump `SNAPSHOT_VERSION`" keeps stock↔fork Full/Diff snapshots byte-compatible both directions → mixed fleets during a roll are safe; no both-hosts-forked gate. Enforced by the `build-firecracker` diff-guard.
- **Build-env (folded into the `build-firecracker` job):** FC pulls `aws-lc-sys` (cmake) + `userfaultfd-sys` (kernel uapi headers + bindgen/clang). The `publish-host-binaries` global `-I/usr/include` CFLAGS can't be reused — it breaks `aws-lc-sys`'s musl build — so expose only the kernel uapi dirs into musl's sysroot. Build only `-p firecracker` (skip jailer/helpers) in `working-directory: third_party/firecracker` so FC's pinned `rust-toolchain.toml` governs.

**Fork-maintenance ergonomics** (a fork is only sustainable if rebasing is automated and breakage is loud): vendored as a git submodule at `third_party/firecracker` (patch surface = a small commit series on the pinned upstream tag); the daily auto-rebase cron (above); the `fc_fork` lane in `.github/scripts/detect-rebake-lanes.py` gating `build-firecracker` + `publish-node-assets`, so bumping the fork is just another change the pipeline rebuilds + rolls.

**Risks (both resolved):** **R1 AGPL** — implemented clean-room from pristine upstream + this ADR/runbook, never the loopholelabs diff; the small surface kept it tractable. **R2 snapshot wire-format skew** — a non-issue *by construction* (above), enforced by the CI diff-guard.

### Phase D — the unified memory substrate

**Status: ⬜ pending — gated on the S0 feasibility spikes (the program's go/no-go).**

The substrate as decided above; this section is the implementation ladder. Each milestone is its own PR chain with an ADR bookend update, validated on the real `engram-dev` KVM box with real Firecracker before merge, with every automatically-testable behavior wired into CI (the FC `#[ignore]` suite runs on Blacksmith).

**S0 — feasibility spikes (throwaway probes on the dev VM; results recorded here before D1 starts).**

S0.0 first: the dev VM kernel must be ≥5.19 for WP-on-shmem (upgrade via Ubuntu HWE or recreate on ubuntu-2404; CI runners are already 2404). Verify GKE COS node kernels (≥6.1 expected) once via prod-ops.

| Spike | Question | Pass criterion |
|---|---|---|
| S0.1 | memfd/tmpfs `MAP_SHARED` + UFFD `MINOR\|WP` registration; `UFFDIO_CONTINUE`; WP fault delivery; `ZEROPAGE`-on-shmem behavior | all ioctls behave per design on the dev kernel |
| S0.2 | the carve: `MAP_FIXED` overlay page over a shared-base mapping mid-run; VMA growth; `vm.max_map_count`; first-write fault cost | correct content post-carve; ~µs-scale; bounded VMA overhead |
| S0.3 | **the go/no-go**: a live KVM guest (minimal, then real FC) with shm-backed memory; carve VMAs under the live memslot; guest correctness + KVM stability; snapshot capture still works | guest reads its private page; no KVM/EPT errors |
| S0.4 | `CONTINUE` throughput + 4 KiB fault-rate vs 512 KiB chunk installs (the resume fault-storm budget) | resume-latency model ≤ today's UFFD path |
| S0.5 | overlay-as-dirty-set correctness, cross-checked against the KVM dirty log | overlay enumerates exactly the dirty pages |

If S0.3 fails hard (KVM will not tolerate carving under a memslot), the documented fallbacks are: per-sandbox shm without the carve (`MINOR`-only — lazy + readable but the clean working set duplicates per session; density cost quantified before accepting), or the interim roadmap under "Interim fallbacks." The S0 probes graduate into a `#[ignore]`'d kernel-capability regression test in CI so a kernel or FC regression is caught on the runners, not in prod.

**D1 — fork v2 (`cortexapps/firecracker`, clean-room).** Construct guest memory from a configured shared base shm object (+ per-sandbox overlay path) on both the boot and restore paths; register UFFD `MINOR|WP`; extend the handler handshake mappings as needed. Snapshot create/read paths verified unchanged; R2 invariants held (never touch `src/vmm/src/snapshot/`, never bump `SNAPSHOT_VERSION`); the v1 `Msync`/`shared` surface repurposed or retired; the rebase cron stays green (the v2 surface remains a small commit series).

**D2 — handler overlay engine (`engram-uffd-handler`).** The riskiest code: a `CONTINUE` arm, a WP/carve arm, overlay lifecycle, lazy base-shm population, the 512 KiB-chunk-fill / 4 KiB-fault granularity policy, the working-set trace + prefault adapted, install-bitmap semantics revisited. Built alongside the existing engine so stock behavior remains until D3; unit tests per arm + an FC integration test in CI.

**D3 — adopt for cold boot.** Parity gates, hard: density ≥ File mode measured on real images on the dev VM; restore latency ≤ today + ε; full FC `#[ignore]` suite + `stock_fork_snapshot_compat` green. Then retire `ENGRAM_FC_BASE_RESTORE_MODE` + the File leg — a clean break.

**D4 — adopt for resume.** Replaces the `UFFDIO_COPY` path; warm overlay/base retention across idle on the host (a cache — correctness never depends on it; reclaim under pressure); **resume host-affinity** (the ADR 0039 deferred item) lands here or earlier as an independent small PR — a soft `prefer_host` tier in `HostRegistry::pick_for_session_inner` (the live picker; `scheduler.rs::pick_host_for_session` is vestigial). Warm same-host resume approaches `CONTINUE` speed; cross-host resume keeps today's chunk path until C adds the peer arm.

**D5 — teardown on the overlay.** Pause → `state.bin` → session `Idle` immediately; background chunk+upload reads the overlay; the PG snapshot row is written only at finalize ("row-only-at-finalize" — no half-durable rows, sidestepping the resume-vs-`recoverable` footgun), with a `session_lease` touch primitive for uploads outliving the 180 s reap and a bounded resume wait on the lease. Failure = no row → resume falls to the prior checkpoint, the same blast radius as an active host death.

### Phase C — post-copy live teleport on the substrate

**Status: ⬜ pending — gated on D3 (the substrate restoring real sessions).**

**Transport.** No host-to-host channel exists today (only coord↔host-agent gRPC), but `hostNetwork` on the node root netns (ADR 0044 K2) makes host-agents mutually L3-routable on node IPs. Add a dedicated **P2P page-transfer listener** (own TCP port, length-prefixed binary framing for throughput — not per-page protobuf; firewall to node-IPs + a HELLO token). The coordinator brokers a `MigrationTicket{source_addr, token, manifests, last_checkpoint}`.

**Wire protocol (clean-room, AGPL-reference-only).** `NEED_AT{offset,len}` (demand fault, each page once) + prefetch hints; the source replies `PAGE{offset,len,sha256,bytes}` or `ALT_SOURCE{offset,len,sha256}` ("already durable in GCS — pull it yourself"). `ALT_SOURCE` is silo's durability shortcut, and our content-addressing makes it *cleaner* than silo's offset-keyed S3 — the chunk hash *is* the GCS key.

**The move, on the substrate.** The destination pre-stages (FC + netns + handler up, base shm already resident — base pages never transfer), then: pause source → move `state.bin` → resume destination; the destination's overlay fills lazily via the two-source race `{p2p.need_at, store.get_chunk}` at the handler's fill seam, first SHA-256-verified wins. The source serves `NEED_AT` from its overlay file plus `process_vm_readv` on the paused FC for in-flight pages, emitting `ALT_SOURCE` for chunks already durable per the last checkpoint. The handoff critical path contains **no memory capture at all**.

**Crash-safety as the tested default.** Every non-`ALT_SOURCE` fault races a GCS fetch *in parallel* with the P2P `NEED_AT`. If the source dies, the GCS arm completes for any page durable at the last checkpoint; the genuinely-lost case (diverged-since-checkpoint, not-yet-transferred, source dead) triggers a **rung-1 rewind** via the existing coherent-checkpoint recovery (`evacuation.rs` rung-1: restore the memory manifest paired with *that checkpoint's own* disk manifest; loss bounded by cadence, never a state-split). Kill-tests at every migration phase gate Phase C.

**The disk dirty-tail.** Post-copy covers memory; the NBD disk's dirty tail since the last flush still rides the pause window (drain under pause; upload off-pause per ADR 0038 B3) — bounded by the continuous disk flush scheduler's cadence. Extending the two-source race to the disk fetch path is an explicit Phase C design decision, not an oversight.

**Drain-seam integration.** Add `migrate_session_live(session, target)` invoked from the admin drain path, feature-gated on both-hosts-substrate-capable, falling back to today's snapshot-rehome drain otherwise — reusing the existing target-pick, PG rebind, and state machine. An opt-in upgrade to an existing seam. The K2 detach/orphan-reap must learn "FC is currently a post-copy source/destination."

**Validation (S-C1-class, replacing the original spike).** On the two-host dev environment (`ENGRAM_INTEG_TWO_HOSTS=1`, real KVM + forked FC): a page dirtied *after* the last checkpoint (so it exists only on the source) demand-faults across the P2P link and the guest continues live on the destination; then the crash arm both ways — source killed, durable page → GCS arm installs it; source killed, non-durable page → clean rung-1 rewind. Plus measured downtime per leg (the honest budget). Kill-tests run in CI where the two-host shape allows; dev-vm-scripted otherwise.

**Risks:** R6 measure real downtime (never quote QEMU's); R7 K2 reap coverage for migration roles; R8 doubled P2P+GCS fetch pressure during a consolidation wave (rate-limit / per-node migration concurrency cap).

### Phase E — true autoscale-down

**Status: ◐ E1 decision engine shipped (#138); the E1 actuator + E2 are pending.** #138 landed the *decision* half: `scaler.rs::desired_hosts` grew a scale-down arm, `reconcile.rs` holds an `Ordering`-counted hysteresis streak, and `AutoscalingSpec.scale_down: Off | IdleOnly | Aggressive` (default `Off`) gates it. It is deliberately **log-only** today — a CONFIRMED scale-down logs *"would drain the least-loaded host and remove its node"* and returns without acting. Pending for E1: the actuator — `CoordClient::list_hosts()` to pick the least-loaded victim, drain that specific node, then `set_size` (order matters; see below). E2 (relax the idle-gate to consolidate active hosts) is gated on Phase C.

**Two stages.** **E1 (fork-independent, ships first):** reuse today's snapshot-and-rehome drain, **gated to idle/low-activity hosts** with hard hysteresis — operator-driven cost reclamation, disruptive enough to keep off active hosts. **E2 (after Phase C):** post-copy on the substrate makes the move a near-live teleport, so the idle-gate + hysteresis relax to consolidate *active* hosts continuously — a config loosening, not new mechanism. The substrate sweetens E2 twice: moves are cheap (only overlays travel) **and** packing more sessions per host is what the density half of the substrate exists for.

**The actuation primitive already exists.** `engram-host-operator`'s `roll_node` is already the drain-gated node-removal sequence (cordon → coord drain → gate on `running_sandboxes → 0` → delete pod → uncordon); scale-down reuses its body. **New pieces:** a scale-*down* arm + hysteresis counter in `scaler.rs` `desired_hosts` (shed only when a whole host of slack persists for N consecutive reconciles, clamped to `capacityFloor`/`minHosts`); a `CoordClient::list_hosts()` for the least-loaded victim (`GET /api/hosts` carries per-host `running_sandboxes`); an `AutoscalingSpec.scale_down: Off | IdleOnly | Aggressive` CRD knob (default `Off`, `IdleOnly` for E1, `Aggressive` for E2). **Order:** drain the specific least-loaded node *before* `set_size` (GKE `set_size` is count-only, so draining makes the drained node the safe one to lose). Scale-up stays immediate; scale-down stays slow (the cold-node problem in reverse — a fresh node pays cold chunk-cache reconstruct).

**Risks:** flapping under bursty demand (ship behind the still-off-by-default K4 actuator until calibrated); re-validate the idle-evict ↔ drain race ADR 0044 K5 caught.

### Phase F — admin + web UX for pause / resume / teleport (the test surface)

**Status: ◐ teleport shipped (#137); pause/resume + the live-path swap are pending.** #137 landed the snapshot-rehome teleport end-to-end: `POST /api/admin/sessions/:id/teleport {target_host_id}` marks the session `Evacuating` with the target pinned in `state.teleport_targets`, the scanner resumes it there, and the web `useTeleportSession` host-picker drives it — prod-validated (session `688c54b0`, `ab021c17 → a51746c8`), which surfaced follow-up F1 (below). Pending: the net-new `pause`/`resume` admin passthroughs, and the live-path swap of the `teleport` verb to `migrate_session_live` once Phase C lands (the UX stays put).

A thin tooling layer to drive and observe the live-migration work by hand. `EvacuateSessionRequest` already reserves a target-host field, so "teleport to a chosen host" is a small extension of the existing evacuate pipeline (pause → flush → snapshot → restore on the chosen peer) — pinning the target instead of letting the scanner pick. So F ships an early, **fork-independent** snapshot-rehome form, then the *same* `teleport` verb swaps its implementation to `migrate_session_live` (Phase C). The UX never changes.

- **Admin endpoints:** `POST /api/admin/sessions/:id/teleport {target_host_id}` (extends `evacuate_session` to honor the reserved target); `POST .../pause` + `.../resume` (net-new thin passthroughs to the FC pause/resume primitive — freeze/unfreeze in place, no state-machine change — to exercise the freeze/flush path). Admin-scoped, enforced server-side.
- **Web UX** (`web/`, TanStack Router, cookie auth, admin-gated): `pauseSession`/`resumeSession`/`teleportSession(target)` in `api.ts`; `usePauseSession`/`useTeleportSession` hooks modeled on `useDrainHost`; a small actions group in `SessionDetail`'s `SessionMeta` — Pause/Resume + a Teleport button opening a host-picker that reuses `useHosts()` + the Fleet `Meter`, filtered to ready hosts ranked by free capacity, with the Fleet drain button's confirm pattern.

**Follow-up F1 — the snapshot-rehome teleport mislabels itself as a host failure (found in prod validation).** The early (snapshot-rehome) teleport resumes the session on the target from its *last durable checkpoint*, which lags the live transcript by up to one checkpoint interval (ADR 0043 P2a cadence). That lag triggers the standard rung-1 rewind, which emits `SessionEvent::RecoveredFromCheckpoint` (`engram-coordinator/src/api/snapshot.rs`, `state.rs`). The web renders that event unconditionally via the `Recovery` card (`web/src/components/session-thread/SystemMessage.tsx`) as **"recovered from a checkpoint after a host failure — ~N events after this point were rolled back."** On a planned operator teleport *no host failed* — the operator deliberately moved a healthy session — so the copy is wrong and alarming. (Observed in prod validation: session `688c54b0` teleported `ab021c17 → a51746c8` showed the host-failure card despite a clean move.) **Fix:** distinguish the *cause* of the rewind. Carry a reason on `RecoveredFromCheckpoint` (`PlannedRelocation` vs. `HostFailureRecovery`) — or thread the teleport's `recovery_epoch` provenance — and branch the web copy: a planned move reads "relocated to a new host — ~N events since the last checkpoint were replayed," keeping the honest rolled-back count (snapshot-rehome genuinely drops the post-checkpoint tail) without the false "host failure" framing. **Note:** Phase C dissolves this for planned moves — live post-copy teleport is lossless (no rewind, no rung-1 event), so the card stops firing on planned relocations entirely and "after a host failure" becomes true by construction (it then only ever appears on genuine unplanned recovery). So F1 is a stopgap copy/cause fix for the snapshot-rehome window; C retires the case it patches.

## Interim fallbacks (recorded, not the plan of record)

The design review found two substrate-independent shortcuts. They are deliberately **not** the roadmap — the substrate is the point — but they are real, verified-in-tree options if S0 kills the design or the program must pause:

1. **readv-sourced teleport (no fork, no substrate).** A paused FC's memory is readable by its parent (the host-agent) via `process_vm_readv`/`/proc/pid/mem`, resolving shared base pages and private COW dirty pages alike, for *both* restore modes. Phase C could ship against this source with the two-source combinator unchanged: pause → final diff capture (O(dirty), ~40–200 ms) or direct readv serving → destination UFFD resume. Sub-second teleport, weeks not months — but it inherits today's density-less resume and leaves the memory paths forked. (Caveats verified: YAMA `ptrace_scope` semantics for parent-reads; FC re-parenting on host-agent restart needs `CAP_SYS_PTRACE` as belt-and-braces.)
2. **Teardown pipeline reorder on the diff-chain (no fork).** Split the host snapshot RPC (`snapshot_begin`/`snapshot_wait`), mark `Idle` after the capture leg, upload in the background under a touched lease, write the PG row only at finalize. Collapses user-visible teardown to ≈ pause+capture today, without the substrate. Subsumed by D5 (which is the same shape with the overlay replacing the FC capture).

## Alternatives considered

- **The original Phase D (`MAP_SHARED` memfile + `Msync` flush) as written.** Rejected: unimplementable against prod's actual memory backings (anonymous UFFD RAM; MAP_PRIVATE base) — see "What changed in this rewrite." Its honest repair forks into either D-lite (eager per-sandbox memfile materialization on the latency path) or the substrate; we chose the substrate.
- **Per-sandbox shm backing without the shared base** (`MINOR`-only, no carve). Simpler fork surface and the handler nearly unchanged — but every session privately duplicates its clean working set, un-shipping the ADR 0022/0039 density win. Kept only as the S0.3 hard-failure fallback, with the density cost to be quantified before accepting.
- **Keep snapshot-and-rehome only (no fork).** Faster, no fork burden — but it's the field's commodity behavior, forecloses live teleport and aggressive autoscale-down, and leaves resume density-less forever. Rejected: the substrate is the whole point of the golden state.
- **Fork-of-the-fork (loopholelabs `main-live-migration`).** Archived, far behind upstream, AGPL question, unrelated experiments. Rejected in favor of building the substrate onto upstream ourselves, clean-room.
- **Block-layer memory faulting (silo's model — memory file as an NBD device, no UFFD).** Rejected: we keep our UFFD handler (the fill seam reuses shipped machinery) and borrow only the protocol/durability patterns; silo is AGPL — design reference to reimplement, never import.
- **Network-disk-backed UFFD memory as the per-fault source** (Hyperdisk/NVMe-oF). Rejected as the hot-path source (~ms random-read vs local-NVMe ~µs); viable only as a durable backstop, which the GCS chunk store already is.
- **KSM for density.** Anonymous-only, scan-based (CPU burn + latency), opt-in per-VMA, and defeated by the substrate's shm backing anyway. The `CONTINUE`-shared base is deterministic sharing, not probabilistic dedup.
- **Far / disaggregated memory** (RDMA paging, CXL.mem): research-grade, needs fabric GCP doesn't standardly offer — parked.

## Invariants / tradeoffs (the landmines)

- **The fork is a standing maintenance cost, now with a bigger surface (v2).** Mitigated by the automated daily rebase + loud-failure ergonomics (shipped, Phase B) and by keeping v2 a small auditable commit series. If upstream's memory backend refactors collide, the rebase cron surfaces it before it rots.
- **Carving under a live KVM memslot is the load-bearing kernel assumption.** S0.3 exists precisely to prove or kill it before any production code; its probe graduates into a CI capability test so regressions surface on the runners.
- **VMA growth is bounded but real.** ~2 VMAs per first-written page; raise `vm.max_map_count` deliberately; watch fault-path latency and `/proc/<pid>/maps` size in D2/D3 gates.
- **shm RAM accounting differs from page cache.** Base shm + overlays are swappable but not reclaimable-on-pressure the way the memfile page cache is; hole-punch + overlay retention policy must be explicit (D4), and ADR 0046's memory reservation must count it once, not twice.
- **Post-copy source-death race (Phase C).** The window where the destination is live but still faulting from a dying source is the hard case — and exactly where the durable GCS snapshot is the fallback silo/Drafter lack. GCS-fallback must be the *tested default* of the fault path; kill-tests gate every phase.
- **Crash-safety is bounded, not transactional.** Recovery is from the last durable flush, loss ≤ one checkpoint interval. The periodic checkpoint (ADR 0043 P2a) stays the always-on backstop.
- **Density and latency parity are merge gates, not aspirations.** D3/D4 do not ship on a regression; the clean break (retiring File/Uffd) happens only after the gates hold on real images on real KVM.
- **AGPL.** silo/Drafter and the loopholelabs FC branch are AGPL — design reference to *reimplement*, never import; the substrate design is derived from kernel primitives + this ADR. License-read before copying a line.
- **Autoscale-down flapping** (Phase E). Scale up fast, scale down slow; hysteresis + the still-off-by-default actuator until calibrated against real load.

## Open questions (each answered by a named gate)

1. **Does KVM tolerate VMA carving under a live memslot?** → S0.3 (the program go/no-go).
2. **What is the real first-write fault cost and VMA budget at production dirty-set sizes?** → S0.2 + D2 gates.
3. **Does the 4 KiB-fault / 512 KiB-chunk granularity policy hold up under a resume fault storm?** → S0.4 + D4 parity gate.
4. **Real teleport downtime on our network/instance types** (never quote QEMU's) → Phase C kill-test measurements.
5. **P2P bandwidth at density** — doubled P2P+GCS fetch pressure during a consolidation wave; rate-limiting / per-node migration concurrency cap? → Phase C/E2.
6. **Overlay/base retention policy under memory pressure** (warm-resume cache vs RAM headroom; hole-punch cadence) → D4.
7. **Upstreaming any of v2** (shm-backed guest memory + MINOR|WP registration is generically useful) — worth an upstream FC conversation once proven? → after D3.

## Sources

Carries forward **ADR 0042** (`docs/adr/0042-substrate-architecture-survey.md` + `0042-substrate-survey-evidence.md`): QEMU post-copy docs + Hines'09; Firecracker snapshot/UFFD docs + discussions #3119/#2938 (FC has no native live migration; UFFD restore is page-source-pluggable); `loopholelabs/drafter` + `silo` v0.2.21 (the unified-device post-copy-with-durable-backstop reference, AGPL — design reference only); REAP/FaaSnap/Catalyzer (working-set prefetch); CodeSandbox engineering blogs (`MAP_SHARED` continuous flush, local-NVMe memory). Kernel references for the substrate: `userfaultfd(2)` + `Documentation/admin-guide/mm/userfaultfd.rst` (MINOR/CONTINUE since 5.13 shmem/hugetlbfs; WP on shmem since 5.19; MISSING scope), `mmap(2)` MAP_FIXED semantics, KVM memslot/GUP behavior under address-space changes. Codebase seams verified against the current tree (the UFFD fill seam, `effective_restore_mode`, the disk flush scheduler, the eviction pipeline + lease, the operator `roll_node` + `desired_hosts`, the node-assets / detect-changes pipeline, the web admin UX). Direct read of the `loopholelabs/firecracker` compare diff happened **only** for the superseded v1 surface (recorded in Phase B history); the substrate (v2) design deliberately derives from kernel primitives alone.

## Status

**Proposed → partially Accepted.** Phase A (#136) and **Phase B** — the fork: port, pipeline, drop-in proof, self-maintaining cron, *and* the prod roll to v1.16.0 — are done and Accepted. The substrate (Phase D) and everything riding it (C, the E1 actuator, E2, F's live path) stay Proposed pending the S0 spikes; the ADR flips fully to Accepted as that commit chain completes.

**Commit chain so far:**
- **#136** — ADR authored + Phase A (retire reactive evac). ✅
- **#137** — Phase F teleport test surface (snapshot-rehome + web host-picker). ◐ (pause/resume + live swap pending)
- **#138** — Phase E1 scale-down decision engine (policy + hysteresis + `ScaleDownMode` knob, log-only). ◐ (actuator pending)
- **#139** — Phase B fork-maintenance ergonomics (consume-seam + `fc_fork` detect lane + daily-rebase cron). ✅
- **#149 / #151–#153** — Phase B fork port + submodule vendor + `build-firecracker` job + stock↔fork compat test. ✅
- **#158 / #159** — Phase B catch-up to upstream **v1.16.0** + the self-maintaining cron (lockstep `engram-v*` tags, version-agnostic R2 guard). ✅ → **prod-rolled 2026-06-09** (fleet on the fork at v1.16.0, all three active images smoke-tested).
- **(this PR)** — the substrate rewrite: Phases C/D re-derived from the unified memory substrate; S0 spike ladder defined; interim fallbacks recorded.

**Next gate:** the **S0 feasibility spikes** (dev VM, real KVM) — S0.3 carve-under-memslot is the program go/no-go; results land back in this document before D1 (fork v2) begins. Open follow-up: [#160](https://github.com/cortexapps/engrams/issues/160) (version-aware base-snapshot reuse), to land before the next `SNAPSHOT_VERSION`-changing FC bump.

Builds on ADR 0042 (survey), ADR 0043 (the shipped no-fork hardening + the precondition Phase 1), ADR 0044 (the K8s fleet whose drain-driven evac Phase A keeps and Phase C upgrades), ADR 0022/0039 (the File-mode density baseline the substrate must meet), and ADR 0028/0038 (the diff-chain durability backstop the substrate keeps).
