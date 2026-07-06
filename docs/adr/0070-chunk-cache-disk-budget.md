# ADR 0070: hard host disk budget for the chunk cache

**Status:** Accepted (2026-07-01) — shipped in four commits on `feat/chunk-cache-disk-budget`
(issue #528): chunk-store budget derivation + pin accounting + periodic sweeper
(`f2968bb0`), UFFD-handler single-evictor (`0ac3f55e`), host-agent wiring
(`2cd69096`) + mountpoint gate (`b7ba7f96`), helm chart (`21f01b2e`).

**Related:** ADR 0007 (chunked-immutable storage — the cache this budgets), ADR 0028
(eviction durability under host roll — the churn this closes a contributor to), ADR 0044
(the K8s host fleet — the kubelet eviction line this stays under), ADR 0039 (cache
locality / pinning — the pin-set semantics this adds arithmetic to). Numbering note: 0060
was previously double-assigned to an earlier draft of this exact topic (chunk-cache
free-floor/kubelet-headroom) and a Slack-bot ADR; the chunk-cache draft was removed in
commit `97b935b6` (issue #450) and its rationale folded into `cache.rs`'s doc comments.
This ADR is the actual, current chunk-cache-budget decision record, numbered fresh.
Renumbered a second time, 0067 → 0070, in the core-ops-batch correction pass: this ADR
landed as 0067 (issue #557), which collided with the pre-existing
`0067-browser-stack-reliability-and-portable-bundle.md`; 0068/0069 were already taken
by the time the collision was caught, so this one moved to the next free number.

## Context

The fleet was being killed by a disk-exhaustion -> kubelet-eviction -> host-churn ->
GCS-cold-path feedback loop. Evidence collected 2026-07-01 (7-14 day Cloud
Logging/Prometheus/PG windows):

- `engram_chunk_cache_size_bytes` reached 182.2 GB on a 298.1 GB disk
  (`chunk_cache_fs_free_bytes` = 86.9 GB) — the cache was the dominant disk consumer.
- Node 611v (host `9723a6a3`) died 2026-07-01 21:21Z at 90.3% disk: kubelet event
  `Evicted ... The node was low on resource: ephemeral-storage`. Node-prep then
  Init:Error-looped every ~15 min, and 9 distinct host-agent pod identities emitted logs
  in the following 48 h — each churn resets Prometheus counters and orphans the local
  chunk inventory.
- 583 `write_local` + 55 write-through-to-cache ENOENT failures in 7 days, concentrated on
  the churning pods — a distinct failure class from the `.partial`-rename race PR #437
  fixed (that class measured zero in this window).
- 7-day resume shape: `recovered_from_checkpoint` (30) >= `resumed` (24) — resume was
  effectively always cold recovery; agent-handshake tail p50 1.34 s / p90 37.1 s / p95
  60.2 s / max 299.2 s. Steady-state GCS miss ratio when a host *survives* is only 2.9%
  (mean fetch 190 ms) — locality works fine when the host lives. Of the 19 sessions
  created in the 2 days around 611v's death, 42% failed/died/were lost, clustered around
  the disk death — the churn is the amplifier, not a uniform failure rate.

The cache already had eviction machinery (a 20% free-space floor since ADR 0060/PR #442,
a refcounted pin set, an optional env ceiling), but four structural holes let the loop
run:

1. **No absolute budget by default.** `ChunkCacheConfig::budget_bytes` defaulted to
   `NO_CEILING`; only the free-space floor governed, and only on hosts where it got a
   chance to run (see #2).
2. **Enforcement was populate-path-only.** The debounced sweep hung off `write_local`; the
   only other callers were batch-closers. A host under disk pressure from non-cache
   writers, or one that just restarted with zero populate traffic, enforced nothing until
   its next chunk write.
3. **A second, pin-blind evictor.** `engram-uffd-handler` ran its own `ChunkCache` over
   the *same* `cache_root` the host-agent pins into, with a stale fixed 200 GiB ceiling
   and an empty pin set. Eviction is oldest-populate-first, so a pressured handler sweep
   preferentially evicted exactly the pinned base-image chunks the host-agent was
   protecting.
4. **Pin arithmetic vs. the budget was unspecified.** Nothing alarmed when the
   enabled-image set's pinned bytes alone approached or exceeded what the disk could
   hold.

## Decision

Make the invariant structural: **at every sweep-interval boundary, `cache_bytes <=
budget_bytes` and `fs_free >= free_floor`, enforced by exactly one evictor per host,
where `budget_bytes` is derived from the disk so it stays strictly below the kubelet
ephemeral-storage eviction threshold — and pins are an unevictable floor whose overflow
alarms instead of silently un-enforcing the budget.**

### 1. Disk-derived default budget + pin-aware arithmetic

`ChunkCacheConfig::from_env_or_default` now derives a real ceiling instead of
`NO_CEILING`:

```
default_budget_bytes(fs_total, fraction, headroom_frac)
    = min(fs_total * fraction, fs_total * (1 - headroom_frac))
```

`fraction` defaults to 0.60 (`ENGRAM_CHUNK_CACHE_DISK_FRACTION` overrides) — the cache's
fair share, leaving room for snapshots/checkpoints/memfiles/OCI-cache/jails/OS.
`headroom_frac` reuses the resolved free-space-floor fraction (default 0.20, same
kubelet-line rationale the floor already documents) as a safety cap: the budget can never
itself authorize filling past the line the floor is trying to hold under. On the 298.1 GB
prod disk this resolves to ~179 GB (vs. the unbounded 182 GB+ the cache had actually
grown to). `ENGRAM_CHUNK_CACHE_BUDGET_BYTES` still wins outright as an absolute operator
override; a filesystem-probe failure falls back to `NO_CEILING` with a warn (fail-soft,
matching the floor's own probe-failure behavior).

`evict_to_budget` now computes `pinned_bytes` (sum of every pinned entry's size) on every
sweep and emits `engram_chunk_cache_pinned_bytes` / `engram_chunk_cache_budget_bytes`
gauges plus `engram_chunk_cache_pins_over_budget` (1 when `pinned_bytes > budget_bytes`,
with a rate-limited-by-sweep-interval `error!`). **Pins are a floor, never a bug**: the
sweep still skips every pinned entry regardless of pressure — the response to pins
exceeding budget is the alarm (more disk, fewer/graded enabled images — see
`enable-fleet-prewarm`, #538), never silently un-pinning or lifting the budget.

`image_prefetch.rs`'s reconcile tick additionally gauges `engram_host_base_memfile_bytes`
— summed size of every materialized per-template base memfile (ADR 0022 residency), which
is unevictable disk in the same category as pinned chunks. `generation-purge` (a later
item in this overhaul) deletes File-mode memfiles entirely, taking this term to zero; this
ADR only measures it.

### 2. Periodic enforcement independent of populate traffic

`ChunkCache::spawn_sweeper(interval)` runs `evict_to_budget` on a timer
(`ENGRAM_CHUNK_CACHE_SWEEP_INTERVAL_SECS`, default 60 s; `0` disables, for tests). The
host-agent holds the returned `JoinHandle` for the process lifetime, the same pattern as
the existing `base_shm_gc::spawn`. This closes hole #2: a host under pressure from
non-cache writers, or a cold-started pod with an empty in-memory pin set, is swept within
one interval regardless of whether anything is being populated. Side benefit: the size/
free/pinned/budget gauges now refresh every interval instead of only on a populate.

The sweeper deliberately skips `tokio::time::interval`'s immediate t=0 tick and waits a
full `interval` before its first sweep. A t=0 sweep runs before the image-prefetch
supervisor's reconcile (which needs a coordinator RPC round trip) has re-established the
in-memory pin set, so on a host that restarts already over budget it would evict
pin-blind — oldest-mtime-first, i.e. exactly the boot-staged base-image chunks pinned in
the prior life — flapping readiness and re-fetching from GCS on every rollout of a host
sitting at or over budget (the first rollout of this ADR does exactly that: prod caches
sit ~182 GB against the ~179 GB derived budget). The populate-path debounced sweep still
bounds growth from writes during that first interval.

### 3. Single evictor

`ChunkCacheConfig::eviction_enabled` (default `true`) lets a cache populate (write,
serve reads) without ever unlinking a file. `engram-uffd-handler` builds its cache with
`eviction_enabled: false` and the `--cache-budget-bytes` flag (plus
`DEFAULT_CACHE_BUDGET_BYTES` and its plumbing through `ChunkedMemoryBackend`) is deleted
outright — zero external users, no compat shim. The host-agent, which holds the pin set,
is the only process per host that evicts from here on; the handler still populates via
write-through (the locality win stays), it just never reclaims. This is explicitly an
interim step ahead of `epic-substrate-single-writer`: that item's Phase 1 assumes this
deletion has already landed and must not re-claim the flag surface.

### 4. Headroom alarm + dedicated volume

The host-agent's heartbeat utilization sampler now emits
`engram_host_disk_headroom_to_kubelet_bytes = fs_free - fs_total *
ENGRAM_KUBELET_EVICT_PCT/100` (env resolved once at `UtilizationProbe` construction).
This alarms on **total** disk pressure — snapshots, memfiles, the OCI cache, anything
sharing the mount — not just the cache's own slice, and is meant to fire before the
kubelet acts (the eviction itself is the amplifier).

`ENGRAM_KUBELET_EVICT_PCT` defaults to 10, matching GKE's documented
`nodefs.available < 10%` hard-eviction default and consistent with the 611v incident's own
arithmetic (31,259,425,233 B free at eviction is ~10.5% of 298.1 GB). **This default is a
placeholder, not a verified prod value** — this OSS repo has no visibility into the actual
nodepool/kubelet flags in the engrams-internal deploy repo (a cluster can override the
default via `--eviction-hard`/`--system-reserved`). The chart's `storage.kubeletEvictPct`
and the code's `KUBELET_EVICT_PCT_ENV_VAR` doc comment both carry an explicit TODO to
confirm and adjust this against the real cluster before treating the headroom alarm's
threshold as trustworthy.

The chart adds `storage.dedicatedDevice` (default `""` = today's hostPath-on-boot-disk
behavior). When set, `node-prep` idempotently formats (only if `blkid` finds no existing
filesystem — never reformats a device with live data) and mounts the device at
`workDirHostPath`, ordered before the base-shm tmpfs step so a nested `uffdBaseDir` mounts
inside the dedicated volume's tree. The chart also sets
`ENGRAM_WORK_DIR_REQUIRE_MOUNTPOINT=true`, and the host-agent hard-fails at boot (before
touching `work_dir` or registering) unless `work_dir` resolves to a distinct filesystem
from a boot-disk reference path (`ENGRAM_HOST_ROOT_REF_PATH`, an `st_dev` comparison,
walking up `work_dir` to the nearest existing ancestor since it may not exist yet on a
fresh host) — guarding the base-shm-startup-race failure class where a rolled pod starts
before the mount is visible and silently shadow-writes the boot disk. The reference path
defaults to `/`, correct on bare metal, but the chart overrides it to a read-only hostPath
mount of the NODE's `/` at `/mnt/host-root`: the host-agent container's OWN `/` is the
pod's ephemeral image overlayfs, which is on a distinct device from EVERY hostPath mount
by construction, so comparing against it would always report "distinct filesystem" and
the gate would never fire (caught in review — see "Post-review fixes" below).

**Framing, stated explicitly:** the dedicated volume moves *accounting*, not bytes. The
kubelet's nodefs signal (the boot disk) stops seeing cache growth, so cache pressure can
no longer trigger pod eviction — but the dedicated disk still fills, and ENOSPC on a
snapshot/state.bin write is still fatal to a session. **The budget (sections 1-3) is the
real mechanism; the volume is defense-in-depth.** Nodepool provisioning of the physical
local-SSD/device is engrams-internal (Terraform) and out of scope here; this repo ships
the chart half + the boot gate.

## Consequences

- The chunk cache can no longer grow unbounded by default; every host now carries a real
  ceiling strictly below the kubelet eviction line, re-enforced every 60 s regardless of
  traffic.
- Exactly one process per host evicts. The UFFD handler's `--cache-budget-bytes` surface
  is gone; nothing else in the tree may re-add a second eviction policy over
  `cache_root` without violating this ADR.
- Pins are now visible: `engram_chunk_cache_pins_over_budget` gives an operator a signal
  when the enabled-image set doesn't fit a host's disk, before it becomes a resume
  incident. This is deliberately an alarm, not an auto-relief valve — the fix is a fleet
  or capacity decision, never a silent budget bump.
- `ENGRAM_KUBELET_EVICT_PCT`'s default is unverified against the real cluster; the
  post-merge follow-up (prod rollout + verification, engrams-prod-ops) must confirm it
  before the headroom gauge's alarm threshold can be trusted operationally.
- The dedicated-volume half is deploy-coordinated: the chart ships inert
  (`dedicatedDevice: ""`) until an engrams-internal nodepool change provisions the device
  and flips the value.

## Divergences from the originating issue

- The issue's plan sketched `default_budget_bytes(fs_total, fraction, headroom_frac)` as
  a free function taking `headroom_frac` as a parameter without specifying its source;
  this ADR/implementation resolves it as the SAME resolved free-floor fraction
  (`resolve_free_floor_pct`) already computed for the floor, rather than introducing a
  second knob — one fewer independently-tunable number to keep in sync with the floor's
  own kubelet-line rationale.
- Metric-value assertions (`engram_chunk_cache_pins_over_budget == 1`, etc.) are covered
  by behavioral unit tests (pinned chunk survives, unpinned evicts) rather than reading
  the gauge back through the `metrics` crate — this codebase has no existing pattern for
  a test-scoped metrics recorder, and the crate's `Gauge` type doesn't expose a `get()`
  accessor. The gauge *values* are a scrape-time/e2e concern, covered by the prod-rollout
  verification step, not a pure unit test.

## Post-review fixes

A deep review pass before undrafting found two real bugs in the shipped mechanics (both
now fixed, in the commit chain on top of the four listed above) plus a stale coordinator
comment:

- **Mountpoint gate was vacuous in the K8s DaemonSet it was built for.** The original
  implementation compared `work_dir`'s `st_dev` against the host-agent container's own
  `/` — always the pod's image overlayfs, always a distinct device from any hostPath
  mount, so the gate reported "distinct filesystem" (and returned `Ok`) unconditionally,
  whether or not the dedicated volume had actually mounted. Fixed by introducing
  `ENGRAM_HOST_ROOT_REF_PATH` (default `/`, unaffected on bare metal) and having the chart
  point it at a read-only hostPath mount of the NODE's `/` (`/mnt/host-root`), added to
  the host-agent DaemonSet only when `storage.dedicatedDevice` is set. The gate's unit
  tests were also host-layout-dependent (implicitly assumed the test tempdir shares `/`'s
  filesystem, true on ubuntu CI runners and macOS's APFS firmlinks but false on any Linux
  box with `/tmp` on tmpfs) — reworked to construct both sides of the comparison
  explicitly under the same tempdir root, deterministic regardless of host layout.
- **The sweeper's first tick fired at t=0, before pins exist.** See the "Periodic
  enforcement" section above — fixed by consuming the interval's immediate first tick
  before entering the sweep loop, so the first real sweep lands at t=`interval` instead of
  t=0.
- `engram-coordinator`'s `--mode=all` wiring comment (dev/e2e single-process path) still
  claimed the pre-this-ADR "defaults to 200 GiB" budget; updated to describe the
  disk-derived default and the `create_dir_all` side effect of `from_env_or_default`.
