# ADR 0126: The reattach generation and the drain before an incompatible upgrade

Status: Proposed (2026-11-04)

Terms used in this document:

- **Reattach**: a new host-agent process adopts the microVMs the previous
  process left running on the node (ADR 0044 K2). An image roll depends on it.
- **Reattach generation**: a number compiled into the host-agent. Two builds
  with the same number can reattach each other's VMs. A build that cannot
  adopt the previous build's VMs has a higher number.
- **Incompatible upgrade**: an upgrade whose target build has a higher
  reattach generation than a host that runs now.
- **Drain**: cordon a host, then evict every session on it to a published
  checkpoint, so the host runs no VM.

## Context

Every release so far assumed that the new host-agent can reattach the old
one's VMs. The operator rolls an image in place: it deletes the host-agent pod
and the successor adopts the running VMs. Nothing moves and nothing rewinds.

ADR 0125 breaks that assumption once. The old build serves each session disk
from an NBD server inside the host-agent process; the new build has no NBD
server and cannot serve those VMs. An in-place roll destroys the disk of every
running session on the node.

Moving sessions across the version change does not work either. A coordinator
and a host on different wire versions refuse each other's calls (the
`WireSkew` refusal), so no coordinator can drive a capture or a teleport on
both an old host and a new one.

The service has live users, and other operators install the charts and
upgrade with `helm upgrade`. An upgrade across such a release must work with
no extra steps, and it must lose only sessions that cannot be checkpointed.

## Decision

1. **A reattach generation.** `engram_protocol::REATTACH_GENERATION` is a
   number compiled into the host-agent. A change that stops a new build from
   adopting the previous build's VMs raises it in the same change. ADR 0125
   raises it to 1; a build before the number existed counts as 0. The host
   reports it at registration and in every heartbeat. The coordinator stores
   it on the host row and shows it in `HostView`.
2. **Drain before an incompatible upgrade.** Both charts carry a
   `pre-upgrade` Helm hook. It runs the target build's drain (the
   `engram-upgrade-drain` crate) against the coordinator that still serves,
   before Helm changes anything. Each chart runs it from an image it already
   deploys: the coordinator chart as `engram-coordinator upgrade-drain`, the
   host-fleet chart as `engram-host-operator upgrade-drain`. So no user sets
   an extra image value, and a mirrored or pinned coordinator image carries
   the drain with it. If every live host reports the
   target's generation, it does nothing. Otherwise it cordons every live host
   (owner `admin`), takes a checkpoint of each active session that has none,
   resumes each parked session, and evicts every session with the no-park
   eviction (`EvictLocal`). It exits when no live host runs a sandbox, and it
   exits non-zero when a session cannot be evicted before its deadline; Helm
   then makes no change. The drain uses only RPCs that the previous release
   already serves.
3. **Roll after the drain.** The host-fleet chart's `pre-upgrade` hook writes
   a marker ConfigMap when it drained. Its `post-upgrade` hook,
   `engram-host-operator upgrade-roll`, then deletes every host-agent pod that
   is not on the DaemonSet's current revision, so the `OnDelete` DaemonSet
   rolls onto the new build. The nodes run no VM, so the roll loses nothing.
   With the operator enabled, the operator rolls the pods and the hook only
   removes the marker.
4. **A generation rise clears the drain's cordon.** When a host registers
   with a higher reattach generation than its row holds, the coordinator
   clears the host's cordon. Only an incompatible upgrade raises the
   generation, and the drain placed that cordon. No state crosses between the
   two charts.
5. **Node prep makes room for the pool.** On the first build of the pool,
   node prep clears the chunk cache directory when the filesystem has less
   free space than the pool's data file needs. The cache is reconstructable
   and no host-agent runs during node prep. The chart's default chunk cache
   fraction drops so that the pool and the cache budget together fit the
   filesystem.

## What a plain upgrade across ADR 0125 does

1. `helm upgrade` of either chart runs the drain against the old coordinator.
   Every session becomes a published checkpoint; a session that has never
   checkpointed gets one first.
2. The coordinator upgrades and runs its migrations.
3. The host-agent pods roll onto the new build on drained nodes. Node prep
   builds the pool, clearing the chunk cache if it must.
4. Each host registers with generation 1; the coordinator clears its cordon.
5. A session resumes on a new host from its checkpoint when its owner returns.

A session that the drain cannot checkpoint or evict stops the upgrade before
any change. A session settled to `dead` by a migration of the release itself
is outside this ADR.

## Consequences

- The next release that changes how a VM's disk or memory is served raises
  the generation, and its upgrade drains with no further work.
- A release that keeps the generation rolls in place as before; the hook is a
  no-op.
- An incompatible upgrade evicts every running session. Users see one
  eviction and one resume per session, not a live move.
- An incompatible upgrade clears every cordon on hosts that roll, including a
  cordon an operator placed by hand before the upgrade.

## Testing

- Unit: the drain's plan for each session state and host mix; the roll's pod
  selection; the coordinator clearing a cordon on a generation rise and only
  then; the generation round-trips through registration (both
  `SimMetadataStore` and Postgres, per the conformance rule).
- Crash state: node prep's room-making on a filesystem with a filled cache
  directory, and on a filesystem that is too small even without it.
- End to end: an upgrade from the previous release to this one on the
  development cluster, with sessions in each state.

## Divergences

- The first coordinator chart ran the drain from the host-operator image at
  the chart's app version. A deployment that pins the coordinator image tag
  then ran an operator image that did not match it, and the hook failed
  before it drained. The drain moved into its own crate, and the coordinator
  binary runs it, so the hook uses the coordinator's own image.
