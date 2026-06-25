# ADR 0060: chunk-cache free-space floor leaves headroom below the kubelet eviction line

**Status:** Proposed

**Related:** ADR 0007 (chunked-immutable storage — the cache this tunes), ADR
0021 P2 (verify-on-populate + the free-space-floor LRU that replaced the fixed
byte budget), ADR 0044 (the K8s host fleet whose kubelet does the evicting).

## Context

A prod host-agent roll wedged the whole fleet. The coordinator had just rolled
to a new `engram_protocol::WIRE_VERSION` (a routine clean-break bump); the
`hf-host-agent` DaemonSet (`OnDelete`, operator-driven node-by-node) was rolling
to match. The roll stalled on one node whose **new** host-agent pod was
repeatedly **kubelet-evicted — `The node was low on resource:
ephemeral-storage`** — so the operator sat at `WaitForReady` forever, the
already-serving nodes never rolled, and they stayed wire-skewed (the scheduler
drops a wire-skewed host, so every session create queued with "no capacity"
despite 0 running sandboxes and ~26 GB of nominally-free RAM). Sessions sat
`queued` to the 30-min timeout.

The mechanical root cause is a **threshold collision**, not a missing reaper.
The chunk cache already self-bounds via an LRU free-space floor
(`ChunkCache`, ADR 0021 P2): it fills the backing filesystem to a target and
then evicts oldest-first. That target was `DEFAULT_FREE_FLOOR_PCT = 0.10` —
"keep ~10% free / hold the mount at/under ~90% full." The kubelet's **default
hard-eviction threshold is also `nodefs.available < 10%`**. So the cache is
permitted to grow right up to the exact line where the kubelet starts evicting
pods. There is **zero headroom** between "cache stops growing" and "kubelet
evicts."

That's tolerable while a node is steady-state (the cache holds at ~90%, the
kubelet's continuous check mostly doesn't fire). It breaks on a **roll**: the
cache lives on the `work` hostPath (`/var/lib/engram`) and *persists across pod
restart by design*, while the incoming pod must stage firecracker/kernel/bundles
into an `engram-assets` **emptyDir** on the **same filesystem**. With the
persistent cache already at the 90% line, the new pod's staging has nowhere to
go → `nodefs.available` drops under 10% → the kubelet evicts the new pod before
it can run and GC its own cache down. Recreating the node (fresh disk) clears it;
deleting the pod does not (the hostPath cache survives).

The lazy enforcement widens the window: the floor is checked on a debounced
populate-path sweep, so the cache can briefly overshoot its target between
sweeps, while the kubelet checks continuously and fires first.

## Decision

Raise the default free-space floor so the **persistent cache holds the disk
below the kubelet's eviction line**, leaving headroom for the per-pod emptyDir
staging a roll requires.

- `DEFAULT_FREE_FLOOR_PCT: 0.10 → 0.20` — the cache holds the backing mount at
  **≤ 80% full**, i.e. ~10 percentage points (~29 GB on the ~291 GB FC hosts)
  of clear runway under the kubelet's 90% line. That covers the incoming pod's
  node-assets staging plus the lazy-sweep overshoot.
- The knobs are unchanged and still win over the default:
  `ENGRAM_CHUNK_CACHE_FREE_FLOOR_PCT` (percent) /
  `ENGRAM_CHUNK_CACHE_FREE_FLOOR_BYTES` (absolute) /
  `ENGRAM_CHUNK_CACHE_BUDGET_BYTES` (absolute ceiling). Prod can pin a different
  value via the host-agent DaemonSet env without a re-bake; the default is just
  the safe-by-construction floor for any host-agent under a default-configured
  kubelet.

This is a tuning change, not a new subsystem: the LRU free-space-floor GC
already exists; the only defect was that its default sat *on* the kubelet line
instead of below it.

## Consequences

- **Smaller hot cache.** Holding the mount at ≤ 80% instead of ≤ 90% shrinks the
  resident chunk working set by ~10% of disk, so a few more reads fall through
  to BlobStorage (GCS). On the FC hosts that is ~29 GB of cache traded for the
  headroom — acceptable: the substrate page-in path is verify-on-populate +
  pinned-base (ADR 0021 P2), and the pinned working set is never evicted, so the
  floor only trims cold tail chunks.
- **Does not, by itself, unstick an already-90%-full node.** The new default
  takes effect once a pod with it is running; a node already at the old 90% line
  when this rolls can still evict the incoming pod before it GCs down. The
  operational rule stands: roll onto disk-healthy nodes, and **recreate** (fresh
  disk) a node that is already near-full rather than pod-deleting it. The current
  fleet has headroom, so the normal re-bake + roll delivers this safely.
- **Not the only gap this incident exposed** (tracked separately, not in this
  ADR): the operator should bound `WaitForReady` and auto-recycle a node whose
  updated pod can't reach Ready instead of stalling indefinitely; and a
  wire-skewed/unschedulable host is dropped silently — a
  `hosts_unschedulable{reason}` signal + a "queued with zero schedulable hosts"
  alert would have surfaced this in seconds rather than at the 30-min timeout.

## Validation

- `cargo nextest run -p engram-chunk-store` — the free-floor resolution +
  `bytes_to_free` floor-pressure tests (the env-override test pins `25` and the
  out-of-range test asserts symbolically against `DEFAULT_FREE_FLOOR_PCT`, so
  both are unaffected by the constant change).
- Prod: the next host-agent re-bake + fleet roll carries the new default; watch
  host `util_disk_used_mib` settle at ~80% of capacity and confirm a roll lands
  the incoming pod without a DiskPressure eviction.
