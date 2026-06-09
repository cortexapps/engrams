# ADR 0045: Live session teleport — post-copy migration, off-pause flush, and true autoscale-down (the substrate golden state)

Status: 2026-06-08 — **Proposed.** A phased roadmap and the home for the fork-gated "golden state" that ADR 0043 deferred. Phase A (retire reactive evac) is no-fork and executes now; Phase B (fork Firecracker) is the irreversible decision gate; Phases C/D (post-copy + `MAP_SHARED`) are gated behind it; Phase E (autoscale-down) and Phase F (the pause/teleport test surface) ship a no-fork form now and a live form after C. Flips toward **Accepted** phase-by-phase as the commit chain lands. Supersedes ADR 0043 Phase 3b (retire evac) and carries forward ADR 0042's Tier-3 "differentiated move."

## Context

The substrate has evolved across three ADRs:
- **ADR 0042** (survey) named the differentiated move: **post-copy live migration with a durable-snapshot fallback** — "teleport" a running session host-to-host with ~7–12 ms downtime, crash-safe because the working set is already durable in GCS. That intersection is unoccupied: silo/Drafter do post-copy *without* a durable fallback; everyone else (E2B, CodeSandbox, Modal, Replit) does snapshot-and-rehome *without* post-copy. Our durable chunk store gives us both.
- **ADR 0043** (roadmap) shipped the no-fork wins — P0 fail-fast, P1 immediate-resume / non-blocking UFFD prefetch, P2a relaxed checkpoint cadence — and explicitly gated the fork.
- **ADR 0044** (shipped) moved the Firecracker host fleet to Kubernetes. Two consequences land here: (1) the **drain-driven evacuation machinery is now load-bearing** — K3 drain = mark `Evacuating` → the `evac_resumer` scanner → resume on a peer — so ADR 0043's "fully retire session evac" can no longer be taken literally; and (2) ADR 0044 K4 shipped demand autoscaling but **deferred drain-gated scale-*down***.

What remains to reach the golden state, and what this ADR commits to:
1. **Retire the *reactive* evac, keep the *drain-driven* evac** (the ADR 0043 Phase 3b reconciliation). The reactive triggers — dead-host auto-evac and the NBD-loss trigger — are the documented bug source (the single-threaded scanner starving on an unbounded dead-host RPC, the resume-from-idle wedge, the deploy-storm cascade). The drain-driven path is the seam Phase C upgrades.
2. **Fork Firecracker** — the only way to get `MAP_SHARED` off-pause flush and post-copy memory migration. Stock FC has no live migration; its only cross-host primitive is snapshot-on-source → restore-on-destination.
3. **Post-copy live migration + durable GCS fallback** — the headline: live teleport, crash-safe.
4. **`MAP_SHARED` continuous off-pause flush** — moves snapshot-save off the pause path and keeps the source memfile continuously consistent (the read-side post-copy depends on).
5. **True autoscale-down** — the payoff that rides the post-copy seam: once "move a session to a peer" is a ~10 ms live teleport, the fleet can continuously self-compact to its cost-optimal node count with zero perceived disruption.

The unifying insight: **"teleport a live session" is the same operation as drain/evac today — `move a session to a peer` — matured from pause-snapshot-restore to live post-copy.** So drain (Phase A keeps it), live migration (Phase C upgrades it), and autoscale-down (Phase E consumes it) are one primitive at increasing maturity. We don't build a separate live-migration subsystem; we upgrade the one seam and everything downstream improves at once.

## Decision

**The golden state (two axes):**
- **Memory:** local-NVMe warm primary; resume the VM *immediately* and prefetch the working set in the background (shipped, ADR 0043 P1); GCS as a durable backstop off the hot path; **(fork)** `MAP_SHARED` continuous dirty-page flush so snapshot-save is off the pause path.
- **Cross-host move without dropping:** **(fork) post-copy** — resume on the destination, demand-fault hot pages P2P from the source, pull cold blocks from GCS in parallel, **with the GCS snapshot as the crash-safe fallback**. This is also what unlocks **true autoscale-down**.

**Honest bounds.** "Without disruption" is ~7–12 ms (QEMU post-copy's downtime; the destination resumes after a brief CPU-state copy, then demand-faults) — seamless for a coding session, not literally zero. "Crash-safe" means *bounded* loss: the durable backstop is a lagging replica (the periodic checkpoint cadence), so a mid-migration crash recovers from the last durable flush, not the exact pre-crash instant. Both are strictly better than the field, which loses the whole guest on a post-copy mid-flight crash.

**Key sub-decisions:**

1. **Reconcile "retire evac" rather than delete it wholesale.** The `Evacuating` state + `evac_resumer` scanner + `evacuate_dead_source` are now the K3-drain engine. Retire only the *reactive* producers of `Evacuating` (dead-host auto-evac, NBD-loss); route a dead/degraded host's recoverable sessions to `Idle` for lazy `/resume` on next access. Keep the scanner + `evacuate_dead_source` as the operator-drain engine.

2. **Fork Firecracker, minimal surface, ported onto upstream.** Port the ~4-file `MAP_SHARED` + `Msync`/`MsyncAndState` snapshot surface onto upstream FC v1.10.x ourselves (not a fork-of-a-fork; not no-fork). Keep the fork surface to the memory backend + snapshot path, track upstream otherwise, automate the rebase, attempt to upstream the generically-useful `MAP_SHARED` dirty-tracking. We already own + build the guest *kernel* (ADR 0025), so adding a built VMM binary is an incremental pipeline step.

3. **Reuse the producer seam.** Post-copy is a producer swap at the single UFFD fault seam (`chunked.rs` `cache.get(hash, || store.get_chunk(hash))`) plus the already-shipped concurrent prefault loop (ADR 0043 P1). The source host becomes another producer; the surrounding singleflight / install-idempotency / hash-verify machinery is reused, not rebuilt.

4. **Crash-safety is the tested default, not a catch block.** GCS is a co-equal, always-armed parallel arm of the fault path; source death → rung-1 rewind via the existing coherent-checkpoint recovery. Kill-tests at every migration phase gate Phase C.

## Phases

| Phase | What | Fork? | Ships |
|---|---|---|---|
| **A** | Retire *reactive* evac (dead-host auto-evac + NBD-loss); keep drain-driven evac; lazy-resume-on-next-access | no | now |
| **B** | Fork Firecracker — port the `MAP_SHARED` + `Msync`/`MsyncAndState` surface onto upstream v1.10.x; vendored + auto-rebased + detect-changes-wired | — | gate |
| **C** | Post-copy live migration + durable GCS fallback (live teleport) | yes | gated on B, D |
| **D** | `MAP_SHARED` continuous off-pause memory flush | yes | gated on B |
| **E** | True autoscale-down — E1 (no-fork, idle-only) now; E2 (aggressive) after C | E2 only | E1 now / E2 gated |
| **F** | Admin + web UX for pause / resume / teleport — the test surface; snapshot-rehome early, live under the same verb at C | no | early |

**Dependencies:** `Phase 1 prefetch (shipped) + B + D → C`. A is independent and first. E1 and F's snapshot-rehome form are fork-independent (ship anytime after A); E2 and F's live path are config/impl upgrades once C lands. **B is the irreversible decision gate** — the S-C1 spike (below) is its go/no-go.

## Phase detail

### Phase A — retire reactive evac (no fork)

Retire the reactive evac, keep the drain-driven evac. **No DB migration** (`Evacuating` + `host_lost` stay valid `sessions.status` values; nothing is removed). **No new resume code** — routing a dead host's recoverable sessions to `Idle` lands them on the existing `/resume` path (`resume_session` → `resume_from_idle`).

- **`dead_host.rs`** — drop `auto_evac_enabled()` + the `ENGRAM_DEAD_HOST_AUTO_EVAC` flag; the detector's stage-2 routing becomes `snapshot.is_some() || has_live_manifest → Idle` (lazy resume; preserves disk-only recoverability via `resume_disk_only_cold_boot`), else `Dead`. The detector itself stays — sessions must not linger `Active` pointing at a dead host.
- **Delete `nbd_loss_trigger.rs`** + its `pub mod` + the heartbeat hook, and remove the now-always-empty `nbd_unhealthy` wire field across `engram-protocol` / `engram-coordinator` / `engram-host-agent` (incl. the never-wired `NbdHealthMonitor`) as one atomic cross-crate commit. The field is `#[serde(default)]` everywhere, so the roll is order-independent.
- **Keep untouched:** `evac_resumer` (the scanner), `evacuate_dead_source` + helpers (still the scanner's relocation primitive), `admin.rs` drain/evacuate/cordon/uncordon, the `HostLost → Evacuating` transition edge (harmless, unused, available to Phase C).
- **Tests:** a live-PG dead-host routing test (recoverable → `Idle` *not* `Evacuating`; non-recoverable → `Dead`) added to `admin_evac_live_pg.rs` (already in CI's live-PG allowlist); existing `evacuate_dead_source` + drain e2e tests stay green.

This is the bounded-crash-window posture ADR 0042/0043 chose for the *unplanned* case (E2B's explicit-durability stance), accepted because the reactive machinery cost more than the durability it bought.

### Phase B — fork Firecracker

**Recommendation: port the ~4-file surface onto upstream FC v1.10.x ourselves.** Not fork-the-loopholelabs-branch (`main-live-migration` is archived 2025-09-22, ~211 commits behind their own main, carries unrelated PVM experiments + an AGPL/licensing question). Not no-fork (stock FC restore is `MAP_PRIVATE`, so an external `MAP_SHARED` mmap never sees the live VM's mutations — a continuously-consistent *source* memfile genuinely requires FC writing the file). The load-bearing diff is small and auditable: a `shared: bool` flag flipping the memory mmap to `MAP_SHARED | MAP_NORESERVE`, an `msync(MS_SYNC)` over the regions, and two `SnapshotType` variants (`Msync`, `MsyncAndState`) — in `vstate/memory.rs`, `vmm_config/snapshot.rs`, `persist.rs`, `vstate/vm.rs`.

**Build pipeline:** a `build-firecracker` job modeled on the existing `publish-host-binaries` lane (musl-static build + `oras` OCI artifact to GHCR); `docker/node-assets-fetch.sh` gains an `FC_SRC`/`FC_BINARY_SOURCE` override (mirroring the kernel's `ENGRAM_KERNEL_SRC`) so it consumes the fork-built binary instead of the upstream release download. The `/opt/engram/firecracker` daemonset contract is unchanged; the per-pod emptyDir means detached live VMs keep their old binary, so a roll is clean and gradual via the HostFleet CR `nodeAssetsImage` digest.

**Fork-maintenance ergonomics** (a fork is only sustainable if rebasing is automated and breakage is loud):
- **Vendored as a git submodule** at `third_party/firecracker` → engrams' fork (`cortexapps/firecracker`), branch `engram-live-migration` = our patch surface as a small commit series on the pinned upstream `v1.10.x` tag. The engrams tree carries only the gitlink + `.gitmodules`; the patch content lives in the fork. (Fallback if commit-series rebasing proves painful: a pristine-upstream submodule + a `third_party/firecracker-patches/*.patch` series applied at build — a cleaner auditable delta for the AGPL review.)
- **Daily auto-rebase cron** (`.github/workflows/rebase-fc-fork.yml`, `on: schedule:` + `workflow_dispatch` — net-new; the repo has no scheduled workflows today): fetch upstream, `git rebase` our commits onto the newest `v1.10.x` tag; **on success** → push + open a PR bumping the submodule gitlink (which trips the `fc_fork` detect-changes lane → node-assets rebuild → host roll) + run the stock↔fork snapshot-compat test; **on conflict / build-fail** → fail loudly: a README CI-status badge for the workflow + an auto-opened/updated tracking issue carrying the rebase output.
- **Detect-changes integration** (FC builds as a first-class input): an `fc_fork` lane in `.github/scripts/detect-rebake-lanes.py` (`FC_FORK_PATHS = ["third_party/firecracker", ".gitmodules"]`), surfaced as a `detect`-job output in `bake-images.yml`, gating `build-firecracker` + `publish-node-assets`, with an `engrams-fc-fork-changed` `repository_dispatch` so a submodule bump flows through the existing node-assets → `nodeAssetsImage` → operator drain-gated roll. Bumping the fork is just another change the pipeline rebuilds + rolls.

**Risks:** **R1 AGPL** — license-read the fork's added commits before copying a line; reimplement clean-room from the public diff if needed (the ~4-file surface makes this tractable). **R2 snapshot wire-format skew** — the fork length-prefixes the state buffer, so stock↔fork restore may be incompatible; test compat in CI and gate migration on both-hosts-forked if so (mixed fleets are guaranteed during a roll).

### Phase C — post-copy live migration + durable fallback

**Transport.** No host-to-host channel exists today (only coord↔host-agent gRPC), but `hostNetwork` on the node root netns (ADR 0044 K2) makes host-agents mutually L3-routable on node IPs. Add a dedicated **P2P page-transfer listener** (own TCP port, length-prefixed binary framing for throughput — not per-page protobuf; firewall to node-IPs + a HELLO token). The coordinator brokers a `MigrationTicket{source_addr, token, manifests, last_checkpoint}`.

**Wire protocol (clean-room, AGPL-reference-only).** `NEED_AT{offset,len}` (demand fault, each page once) + prefetch hints; the source replies `PAGE{offset,len,sha256,bytes}` or `ALT_SOURCE{offset,len,sha256}` ("already durable in GCS — pull it yourself"). `ALT_SOURCE` is silo's durability shortcut, and our content-addressing makes it *cleaner* than silo's offset-keyed S3 — the chunk hash *is* the GCS key.

**Producer swap at the one seam.** `engram-uffd-handler/src/chunked.rs` `fetch_chunk` already resolves a fault via `cache.get(hash, || store.get_chunk(hash))` — a closure wrapped in singleflight + local-NVMe + cancel-safety. In migration mode, swap that closure for a two-source race: `ALT_SOURCE → GCS only`; else `race { p2p.need_at(offset), store.get_chunk(hash) }`, first SHA-256-verified wins. The surrounding machinery — singleflight dedup, the `installed` bitmap, hash-verify, and the **already-shipped concurrent prefault loop** (ADR 0043 P1) — is *why the swap is small*: a producer change, not a rewrite. The destination keeps `RestoreMode::Uffd`.

**Source side.** After pause + authority handoff the source becomes a page server, `pread`ing the `MAP_SHARED` memfile (Phase D — why C depends on D) to answer `NEED_AT`, emitting `ALT_SOURCE` for any chunk already durable per the last checkpoint.

**Crash-safety as the tested default.** Every non-`ALT_SOURCE` fault races a GCS fetch *in parallel* with the P2P `NEED_AT`. If the source dies, the GCS arm completes for any page durable at the last checkpoint; the genuinely-lost case (diverged-since-checkpoint, not-yet-transferred, source dead) triggers a **rung-1 rewind** via the existing coherent-checkpoint recovery (`evacuation.rs` rung-1: restore the memory manifest paired with *that checkpoint's own* disk manifest; loss bounded by cadence, never a state-split). Kill-tests at every migration phase gate Phase C.

**Drain-seam integration.** Add `migrate_session_live(session, target)` invoked from the admin drain path, feature-gated on both-hosts-post-copy-capable, falling back to today's snapshot-rehome drain otherwise — reusing the existing target-pick, PG rebind, and state machine. An opt-in upgrade to an existing seam.

**Go/no-go spike (S-C1).** A single page demand-faulted across two `engram-dev` hosts: forked FC both sides, source serving from a *frozen* snapshot (skip D), destination resumes immediately with the two-source combinator; prove exactly one `NEED_AT` round-trips a page and the guest continues live on the destination — then prove the crash arm both ways (durable → GCS installs it; not-durable → clean rung-1 rewind). This single-fault-plus-crash-fallback is the whole go/no-go.

**Risks:** R3 source consistency without D (frozen-snapshot fallback for the spike); R6 measure our real downtime (~10 ms is QEMU's); R7 the K2 detach/orphan-reap must cover "FC is currently a post-copy source/destination"; R8 doubled P2P+GCS fetch pressure at density.

### Phase D — `MAP_SHARED` continuous off-pause flush

A naive FC memory snapshot is I/O-bound at ~1 s/GB (8 GB ≈ 8 s) on the pause path. mmap-ing the memory file `MAP_SHARED` lets the kernel flush dirty pages continuously, cutting snapshot-*save* to 30–100 ms and moving the bulk of I/O off the pause path (CodeSandbox's technique). Requires Phase B's `Msync`/`MsyncAndState`.

**Coexistence with ADR 0022 (the central decision).** `MAP_SHARED` is the **write/save path** on the *live* per-sandbox memfile; ADR 0022's `MAP_PRIVATE` stays the **read/restore path** on the *immutable base* memfile. Different inodes, opposite directions — **no interaction**, and `MAP_SHARED` must not leak into the restore path (ADR 0043's "keep the restore-mode split as-is" still holds; the only resume-side change remains P1's non-blocking prefetch).

**Wiring.** A new `MemoryFlushScheduler` mirroring `engram-host-agent/src/disk_daemon/flush_scheduler.rs` (periodic tick OR dirty-threshold `Notify`, own env vars + kill-switch, `Drop`-aborts handle ordered-first), firing `Msync` instead of a full `create_snapshot`; the pause-path checkpoint gains an `Msync` arm so "save" collapses to `msync` + the small `state.bin`. It wires *into* the existing checkpoint driver, not replacing it (keep the ADR 0038 "skip if capture in flight" guard). **Fragmentation:** the cache lives on `/var/lib/engram` = the GKE node's local disk (COS defaults to **ext4**, not xfs), so CodeSandbox's "fragments xfs fast" caveat likely doesn't bite — but confirm on the real `n2-standard-8` pool; mitigate with `fallocate`-preallocate + the kill-switch (revert to pause-path `Full`/`Diff` capture instantly).

### Phase E — true autoscale-down

**Two stages.** **E1 (fork-independent, ships first):** reuse today's snapshot-and-rehome drain, **gated to idle/low-activity hosts** with hard hysteresis — operator-driven cost reclamation, disruptive enough to keep off active hosts. **E2 (after Phase C):** post-copy makes the move a ~10 ms live teleport, so the idle-gate + hysteresis relax to consolidate *active* hosts continuously — a config loosening, not new mechanism.

**The actuation primitive already exists.** `engram-host-operator`'s `roll_node` is already the drain-gated node-removal sequence (cordon → coord drain → gate on `running_sandboxes → 0` → delete pod → uncordon); scale-down reuses its body. **New pieces:** a scale-*down* arm + hysteresis counter in `scaler.rs` `desired_hosts` (shed only when a whole host of slack persists for N consecutive reconciles, clamped to `capacityFloor`/`minHosts`); a `CoordClient::list_hosts()` for the least-loaded victim (`GET /api/hosts` carries per-host `running_sandboxes`); an `AutoscalingSpec.scale_down: Off | IdleOnly | Aggressive` CRD knob (default `Off`, `IdleOnly` for E1, `Aggressive` for E2). **Order:** drain the specific least-loaded node *before* `set_size` (GKE `set_size` is count-only, so draining makes the drained node the safe one to lose). Scale-up stays immediate; scale-down stays slow (the cold-node problem in reverse — a fresh node pays cold chunk-cache reconstruct).

**Risks:** flapping under bursty demand (ship behind the still-off-by-default K4 actuator until calibrated); re-validate the idle-evict ↔ drain race ADR 0044 K5 caught.

### Phase F — admin + web UX for pause / resume / teleport (the test surface)

A thin tooling layer to drive and observe the live-migration work by hand. `EvacuateSessionRequest` already reserves a target-host field, so "teleport to a chosen host" is a small extension of the existing evacuate pipeline (pause → flush → snapshot → restore on the chosen peer) — pinning the target instead of letting the scanner pick. So F ships an early, **fork-independent** snapshot-rehome form, then the *same* `teleport` verb swaps its implementation to `migrate_session_live` (Phase C). The UX never changes.

- **Admin endpoints:** `POST /api/admin/sessions/:id/teleport {target_host_id}` (extends `evacuate_session` to honor the reserved target); `POST .../pause` + `.../resume` (net-new thin passthroughs to the FC pause/resume primitive — freeze/unfreeze in place, no state-machine change — to exercise the freeze/flush path). Admin-scoped, enforced server-side.
- **Web UX** (`web/`, TanStack Router, cookie auth, admin-gated): `pauseSession`/`resumeSession`/`teleportSession(target)` in `api.ts`; `usePauseSession`/`useTeleportSession` hooks modeled on `useDrainHost`; a small actions group in `SessionDetail`'s `SessionMeta` — Pause/Resume + a Teleport button opening a host-picker that reuses `useHosts()` + the Fleet `Meter`, filtered to ready hosts ranked by free capacity, with the Fleet drain button's confirm pattern.

**Follow-up F1 — the snapshot-rehome teleport mislabels itself as a host failure (found in prod validation).** The early (snapshot-rehome) teleport resumes the session on the target from its *last durable checkpoint*, which lags the live transcript by up to one checkpoint interval (ADR 0043 P2a cadence). That lag triggers the standard rung-1 rewind, which emits `SessionEvent::RecoveredFromCheckpoint` (`engram-coordinator/src/api/snapshot.rs`, `state.rs`). The web renders that event unconditionally via the `Recovery` card (`web/src/components/session-thread/SystemMessage.tsx`) as **"recovered from a checkpoint after a host failure — ~N events after this point were rolled back."** On a planned operator teleport *no host failed* — the operator deliberately moved a healthy session — so the copy is wrong and alarming. (Observed in prod validation: session `688c54b0` teleported `ab021c17 → a51746c8` showed the host-failure card despite a clean move.) **Fix:** distinguish the *cause* of the rewind. Carry a reason on `RecoveredFromCheckpoint` (`PlannedRelocation` vs. `HostFailureRecovery`) — or thread the teleport's `recovery_epoch` provenance — and branch the web copy: a planned move reads "relocated to a new host — ~N events since the last checkpoint were replayed," keeping the honest rolled-back count (snapshot-rehome genuinely drops the post-checkpoint tail) without the false "host failure" framing. **Note:** Phase C dissolves this for planned moves — live post-copy teleport is lossless (no rewind, no rung-1 event), so the card stops firing on planned relocations entirely and "after a host failure" becomes true by construction (it then only ever appears on genuine unplanned recovery). So F1 is a stopgap copy/cause fix for the snapshot-rehome window; C retires the case it patches.

## Alternatives considered

- **Keep snapshot-and-rehome only (no fork).** Faster, no fork-maintenance burden — but it's the field's commodity behavior (real per-move downtime), forecloses live teleport and aggressive autoscale-down, and leaves us paying full pause-capture-restore on every host move. Rejected: the fork is the whole point of the golden state, and we already own the guest-kernel pipeline.
- **Fork-of-the-fork (loopholelabs `main-live-migration`).** Archived, far behind upstream, AGPL question, carries unrelated experiments. Rejected in favor of porting the minimal surface onto upstream ourselves.
- **Block-layer memory faulting (silo's model — memory file as an NBD device, no UFFD).** Rejected: we keep our UFFD handler (the producer swap is small and reuses P1) and borrow only silo's protocol/durability patterns; a block-layer rewrite throws away working machinery. silo is AGPL — design reference to reimplement, never import.
- **Network-disk-backed UFFD memory as the per-fault source** (Hyperdisk/NVMe-oF). Rejected as the hot-path source (~ms random-read vs local-NVMe ~μs); viable only as a durable backstop, which the GCS chunk store already is.
- **Far / disaggregated memory** (RDMA paging, CXL.mem): research-grade, needs fabric GCP doesn't standardly offer — parked.

## Invariants / tradeoffs (the landmines)

- **The fork is irreversible-ish and a standing maintenance cost.** Mitigated by the minimal surface + the automated daily rebase + loud-failure ergonomics (Phase B). If upstream diverges hard, the rebase cron surfaces it before it rots.
- **Post-copy source-death race (Phase C).** The window where the destination is live but still faulting from a source that dies is the hard case — and exactly where our durable GCS snapshot is the fallback silo/Drafter lack. GCS-fallback must be the *tested default* of the fault path, not an afterthought; kill-tests at every phase gate the merge.
- **Crash-safety is bounded, not transactional.** Recovery is from the last durable flush, with loss ≤ one checkpoint interval. Keep the periodic checkpoint (ADR 0043 P2a) as the always-on backstop; it is *the* thing that makes our post-copy crash-safe where the field's is not.
- **Mixed-fleet snapshot compat during a fork roll** (R2). Gate live migration on both-hosts-forked if stock↔fork restore is incompatible; the CR digest roll is gradual, so mixed fleets are guaranteed transiently.
- **AGPL.** silo/Drafter and the loopholelabs FC branch are AGPL — design reference to *reimplement* (`ALT_SOURCE`/two-source combinator post-copy-with-parallel-cold-pull), never import. License-read before copying a line.
- **Autoscale-down flapping** (Phase E). Scale up fast, scale down slow; hysteresis + the still-off-by-default actuator until calibrated against real load.

## Open questions

1. **Real downtime on our network/instance types** — the ~7–12 ms is QEMU's; measure ours in S-C1 (Open Q drives whether "seamless" holds for active sessions).
2. **`MAP_SHARED` fragmentation on the actual `n2-standard-8` cache FS** — confirm ext4 (likely) vs xfs, and the write-amplification budget when the memory flush competes with the disk flush scheduler on the same mount.
3. **P2P bandwidth at density** — the doubled P2P+GCS fetch pressure during a consolidation wave; does it need rate-limiting / a per-node migration concurrency cap?
4. **Submodule vs patch-series** for the fork vendoring — start with the commit-series submodule; fall back to a pristine-upstream submodule + `*.patch` series if rebasing the commit series proves painful (cleaner AGPL-review delta either way).
5. **Upstreaming `MAP_SHARED` dirty-tracking** — worth an upstream FC PR to shrink the fork surface long-term?

## Sources

Carries forward **ADR 0042** (`docs/adr/0042-substrate-architecture-survey.md` + `0042-substrate-survey-evidence.md`): QEMU post-copy docs (downtime ~7–12 ms) + Hines'09; Firecracker snapshot/UFFD docs + discussions #3119/#2938 (FC has no native live migration; UFFD restore is page-source-pluggable); `loopholelabs/drafter` + `silo` v0.2.21 (the unified-device post-copy-with-durable-backstop reference, AGPL); REAP/FaaSnap/Catalyzer (working-set prefetch); CodeSandbox engineering blogs (`MAP_SHARED` continuous flush, desparsification, local-NVMe memory). Direct read of the `loopholelabs/firecracker` `main...main-live-migration` compare diff (the ~4-file `MAP_SHARED` + `Msync`/`MsyncAndState` surface; branch archived 2025-09-22). Codebase seams verified against the current tree (the UFFD producer seam, the disk flush scheduler, the operator `roll_node` + `desired_hosts`, the node-assets / detect-changes pipeline, the web admin UX).

## Status

**Proposed.** Phase A (retire reactive evac) executes now (no-fork); the commit chain is appended and the ADR flips toward Accepted phase-by-phase. Builds on ADR 0042 (survey), ADR 0043 (the shipped no-fork hardening + the precondition Phase 1), and ADR 0044 (the K8s fleet whose drain-driven evac Phase A keeps and Phase C upgrades).
