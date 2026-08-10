# ADR 0115: aux bundle generations are durable at birth, and the pin set covers every live reference

Status: Proposed

## Context

On 2026-08-10 the `chain_poisoned` guard fired in production. The chain of
events (session `e83383b2`, sandbox `768b22e7`):

1. A fresh session create swapped the VM's `dyn_0` aux drive to the host's
   current stamp generation (`fc.swap_aux_bundles`, ADR 0035 §3).
2. A deploy rolled the host-agent DaemonSet. The new pod's init container
   staged a new stamp; the swapped-in generation rotated out of
   `current.json`.
3. The new pod's bundle supervisor swept the node's only copy of that
   generation. The sweep keep-set is `live_bundles` (the coordinator's
   `bundle_pin_set`) ∪ the boot-time stamp; the running VM's attachment is
   in neither.
4. The generation had never reached blob storage: `BundleStore::publish`
   runs only at snapshot time, and no snapshot had referenced it yet.
5. The VM's first periodic checkpoint could not make its pinned generation
   durable (HEAD miss in blob storage, staged file gone). The Diff capture
   had already consumed the KVM dirty bitmap, so the checkpoint chain was
   poisoned. The gate held; no corrupt data was served.

Two structural defects compose here:

- **Durability is circular.** A stamp generation becomes durable only via
  the first snapshot that pins it, but that snapshot needs the bytes to
  still exist. Between create and first checkpoint the bytes are
  single-copy on node-local disk. The catalog producers do not have this
  defect: `RegisterSkill` and `RegisterHarness` both upload to blob
  storage before they write the catalog row. The baked stamp path is the
  one producer class that skips the upload (bake → GHCR → node-assets
  image → hostPath, never blob storage).
- **The pin set is incomplete.** `bundle_pin_set` = snapshot rows ∪
  `mount_catalog` ∪ `harness_catalog`. A generation attached to a running
  sandbox, or staged as a live host's current stamp, pins nothing. Both
  the host sweep and the coordinator bundle GC key off this set, so both
  can reclaim bytes that a live VM's next snapshot needs.

## Decision

The invariant this ADR establishes: **a bundle generation that anything
can reference is durable in blob storage, and every live referent is
visible to the pin set.**

### D1: publish stamp generations at host-agent startup (PR 1)

At host-agent startup, after `read_stamp`, a background task runs the
existing idempotent `BundleStore::publish` over the stamp's generations
and retries on the supervisor's 60 s cadence until it succeeds. The task
does not gate host readiness: a blob-storage outage must not stop the
host from serving, and the snapshot-time publish remains the load-bearing
gate. In the common case snapshot-time publish degrades to a HEAD check.

The publish point is the host, not CI: the OSS bake publishes to GHCR
only and holds no deployment blob-storage credentials. N hosts race the
same content-addressed sha; `publish` is HEAD-first, so the race costs
N HEADs and at most one PUT per generation.

### D2: complete the pin set (PR 2)

- The host heartbeat reports, per running sandbox, the aux bundle refs
  the live `SandboxSpec` actually has attached (the same source the
  snapshot pipeline reads). The field is optional on the coordinator
  side: an old host during a roll reports nothing, which is covered by
  the GC's 24 h grace.
- The coordinator persists the report on the `hosts` row
  (`sandbox_bundles jsonb`, following the `current_bundles` precedent)
  and `bundle_pin_set` gains two legs: per-sandbox attachments and
  current stamps, both over non-dead hosts.
- Ordering: the heartbeat handler persists the report **before**
  computing `live_bundles` for the ack, so a freshly rolled host's first
  ack already protects its own reattached sandboxes from its own first
  sweep. This ordering is the exact window the incident rode.
- `run_one_bundle_sweep` itself does not change; a complete pin set makes
  it correct as-is.

### D3: make the class simulable (follow-up)

The DST worlds cannot represent this incident (empty `aux_bundles`
everywhere, no host sweep actor, cosim uses two disjoint blob buckets).
A follow-up extends the cosim with a shared blob bucket, a real
`BundleStore` + stamp on the sim host, heartbeat/GC/stamp-rotation steps,
and the oracle *every generation attached to a live sandbox is reachable
from local staging ∪ blob storage*. It lands after D1/D2 (the oracle is
red before them). The ADR 0098 D4 conformance obligations for the
`bundle_pin_set` change land inside PR 2, not the follow-up.

## Consequences

- One PUT per generation per bake, paid at host startup instead of at an
  unpredictable first-snapshot. Blob storage now holds every generation a
  fleet host stages, not only snapshot-referenced ones; the GC reclaims
  them once they leave every stamp, every live sandbox, and every
  snapshot.
- The heartbeat grows a per-sandbox field and the `hosts` table a jsonb
  column. Wire skew during a roll is tolerated by serde default plus GC
  grace.
- The sweep comment in `bundles.rs` ("nothing that needs re-opening is
  ever swept") was falsified by the incident and is rewritten under the
  new invariant.
