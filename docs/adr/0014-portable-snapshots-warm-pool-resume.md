# ADR 0014: Portable snapshots — warm-pool session create + durable resume

Status: accepted, 2026-05-15. Revised 2026-05-16 to make the warm-
pool restore path harness-agnostic (option D) and to bolt on the
prefetch + working-set machinery the chunked-memory primitive needs
to actually hit sub-1 s TTFM on the warm path.

Phase: 1 (M1 landed; option D + prefetch follow-ups in flight after
prod metrics showed agent_handshake at p99 ~25 s dominating cold
boot). Commit chain:

- M1.0 `ccde296` — ADR
- M1.1 `5188751` + `af93584` — `paths.rs` canonical-jail contract
- M1.2a `e325fe8` — canonical rootfs/harness paths outside the jail
- M1.2b `0d7341f` — portable state.bin + sidecar upload
- M1.3 `ef033cd` — bake-time template snapshots in image-builder
- M1.4 `83dd40c` — `templates` table + resolver
- M1.5 `5d66696` — gRPC LeaseWarm/LaunchWarm/ListWarmSlots
- M1.6 `594225b` — `WarmPool` component + refill loop
- M1.7 `b195878` — heartbeat warm-slot inventory + active-template push
- M1.8 `4e61699` + `2e1a727` — scheduler warm-lease path + tests
- M1.9 `e88737d` — lease-rate autoscaler
- M1.10 `dc054d8` — serial multi-restore + warm-pool-memory scaffold
- Hotfix `34b18aa` — atomic `Entry` race guard in `maybe_refill`
  (concurrent refills collided on the source-keyed vsock UDS;
  caught by dev-VM e2e, not unit tests)
- Hotfix `e96842d` — no-harness sessions warm-lease via `"none"`
  sentinel uri instead of short-circuiting on `harness_pack_uri=None`
- Dev tool `a803fda` — `seed_warm_template` bin: the missing bake →
  `templates` row glue, see M1.11 below
- `8e219d8` — per-phase cold-boot histograms on the host-agent
  (image_resolve / materialize / fc_boot / agent_handshake /
  create_total). Cold-boot data from prod motivated the M1.11–M1.15
  follow-up work below.
- `9580e83` — warm-path histograms wired with `kind=warm/cold` split
  (dormant until M1.11 lands and warm pool actually fires in prod)
- `3c7cd8e` — `FirecrackerClient::patch_drive` +
  `load_snapshot_paused` + `tests/patch_drive_swap.rs` integration
  test, validating the option-D mechanism on the dev VM

Queued (in dependency order):

- **M1.11** — image enablement → templates row cascade. Hooks the
  existing `POST /api/enabled-images` handler so warm-pool starts
  firing in prod without operator-side scripting.
- **M1.12** — option D itself. Bake at pre-mount + bootstrap
  mounts /dev/vdb + warm-restore PATCHes the harness drive +
  drop `harness_pack_uri` from `templates` unique key.
- **M1.13** — eager-prefetch refill. Refill loop parallel-fetches
  all memory chunks to NVMe before publishing the slot.
- **M1.14** — working-set recording + replay (REAP-style) with
  **synthetic profiling** to capture the kernel pages touched
  during the post-resume mount(2) + execve path.
- **M1.15** — first-class `snapshot_chunk_cache` with pinning
  hints, LRU, metrics.

M2 (durability) is still future work. Concurrent N>1 restores still
require per-FC mount-namespacing (see "Per-FC mount namespace for
concurrent restores" below); v1 effective depth stays at 1/template
/host.

## Context

Two problems with the same root cause.

**Session create is slow.** Every session today is a cold microVM boot.
First-touch per template per host pays OCI pull → chunk + upload → cold
kernel boot with serial NBD-chunked rootfs reads → engram-init →
bootstrap → harness. We measured 30–75 s on the cold path in production
during ADR 0013 rollout. Steady-state (chunks already in GCS, kernel
binary local) is still ~5–15 s, bounded by the kernel boot + vdb ext4
mount, which is fundamentally serial. The 100 ms / sub-second session-
create promise the chunked-memory substrate was built for has never
been delivered, because we built the substrate (chunked manifests, UFFD
prefault, canonical-mmap sharing) but never the driver that consumes it.

**MIG rolls lose session state.** GCE sends SIGTERM; `engram-host-
agent/src/shutdown.rs` snapshots each sandbox to local NVMe and writes
`last_local_snapshot` to `sandbox.json`. The host then dies and takes
the local NVMe with it. `dead_host.rs` flips affected sessions to
`Failed`. The user loses workspace + harness state. Conversation
history is durable in `session_events`; workspace and agent process
state are not. Rolls happen on every image bake, every template
change, every autoheal cycle.

Both problems share one missing primitive: **a snapshot artifact that's
portable across hosts.** Warm-pool wants to fan out one bake-time
template snapshot to N hosts. Durability wants to migrate one session
snapshot from a dying host to a new one. Same operation, different
trigger.

The infrastructure carries us most of the way:

- FC `snapshot()` / `restore()` are wired end-to-end with UFFD chunked-
  memory; restore wall-clock is ~100–500 ms (`crates/engram-sandbox-
  firecracker/src/lib.rs:1250-1379`).
- Bake-time canonical capture exists (`engram-image-builder`
  `CanonicalCaptureConfig`) — it boots a rootfs under FC, settles,
  snapshots, chunks memory, stores ref in `bundle.json`.
- `engram-bootstrap` is a **long-lived supervisor** on vsock 1025
  (`crates/engram-bootstrap/src/main.rs:178`). It loops on `accept()`
  and `cmd.spawn()`s the agent. Bootstrap survives every snapshot/
  restore boundary. After restore, the host dials, pushes a fresh
  `BootstrapLaunch{argv, env}`, bootstrap reaps the prior agent child
  and spawns the new one. Same rehydration path for warm-launch and
  Resume.
- Memory chunks are already in `BlobStorage` (chunked manifests, ADR
  0007). FC `state.bin` and the sidecar JSON are local-only and need
  to become portable.
- Canonical memory uses `mmap(MAP_PRIVATE)` (not KSM) — N concurrent
  VMs of the same template share the canonical mmap via the kernel
  page cache. This is the cost mechanism that makes warm-pool depth
  affordable.

## Decision

Single ADR, two milestones, substrate-only snapshot semantics.

**Substrate-only snapshots.** A snapshot captures kernel + bootstrap +
page caches + filesystem. The agent process is **not** preserved
across thaw. Resume cold-restarts the agent against on-disk transcript
via a fresh `BootstrapLaunch`. This avoids the TCP-streams-break-on-
thaw failure mode (Claude API keepalive state, coord WS to harness, all
broken on thaw) and unifies the rehydration path between warm-pool
launches and durable Resume. Honest UX contract: "Last active <Xh>
ago — Resume" restarts the agent against persisted state, not a frozen
process.

**M1 ships first: warm-pool.** Validates the portable-snapshot
primitive on the simpler payload (no live agent, no per-session
writable-rootfs delta). Hosts maintain a free-list of pre-restored
microVMs per template, refilled async. Session create leases from the
pool when available; cold-create is the fallback.

**M2 lands after M1 has soaked: durability.** Layers durability
semantics on the validated primitive. Active sessions snapshot on
idle-entry and on graceful drain; artifacts upload to BlobStorage. New
`SessionStatus::Paused` + dead-host reaper transitions affected
sessions; `POST /api/sessions/:id/resume` cold-restarts the agent.

The two milestones land sequentially because warm-pool's payload is
strictly simpler — no live-agent state, no writable-rootfs delta — and
exercises the cross-host restore path on the cheapest possible
artifact. Durability piggy-backs on the validated primitive plus a
writable-rootfs upload extension and the Paused/Resume orchestration.

## Architecture

### The portable snapshot primitive

A `PortableSnapshotRef` is the durable handle any coord pod can hand
to any host to restore:

```
PortableSnapshotRef {
  snapshot_id: SnapshotId,
  memory_manifest:           ManifestRef,        // chunked, in BlobStorage
  canonical_memory_manifest: Option<ManifestRef>,// template canonical
  rootfs_manifest:           Option<ManifestRef>,// chunked disk (when Phase 4 NBD writable lands)
  rootfs_blob_key:           Option<String>,     // M2 interim: tar+zstd writable rootfs
  state_blob_key:            String,             // FC state.bin (small, ~tens KiB)
  sidecar_blob_key:          String,             // FcSnapshotManifest JSON
  template_ref:              Option<TemplateRef>,// which template this is restorable as
}
```

Two new BlobStorage object kinds: `snapshots/<id>/state.bin` and
`snapshots/<id>/sidecar.json`. Opaque blobs, not chunked — both small.

Cross-host restore on the receiver:

1. Download state.bin + sidecar.json from BlobStorage.
2. Memory: existing `materialize_memory_if_missing`
   (`crates/engram-host-agent/src/pooled_backend.rs:973-990`) hydrates
   memory.bin from chunked manifest.
3. Rootfs: chunked manifest if present, else tar+zstd unpack to
   canonical jail path.
4. Path canonicalization (below).
5. Hand to existing `FirecrackerBackend::restore_in_jail`.

### Path canonicalization (correctness prerequisite for M1)

`state.bin` embeds `vsock_uds_path`, TAP name, rootfs `path_on_host`.
FC has no API to rewrite state.bin after capture. The existing restore
code already re-provisions TAP at the manifest-derived name
(`reserve_restored_net`, `lib.rs:1280`) and re-creates the vsock UDS at
the manifest-derived path. The canonical scheme tightens the contract:

- All hosts use identical `<work_dir>` (packer-installed:
  `/var/lib/engram/sandboxes`).
- Jail dir: `<work_dir>/<sandbox_id>/` — per-FC-process runtime
  state (FC api socket, log, uffd uds). Removed on destroy.
- vsock UDS: `<work_dir>/<sandbox_id>.vsock` — outside the jail, so
  destroy's `remove_dir_all(jail_dir)` doesn't break a subsequent
  restore.
- **Rootfs canonical path: `<work_dir>/rootfs/<sandbox_id>.dev`** —
  outside the jail, source-sandbox-id-keyed. Symlink to the actual
  rootfs source (NBD device, materialized file, etc.). FC put_drive
  receives this path; state.bin embeds this path. The path
  survives destroy of the source sandbox so cross-host restore (or
  same-host idle resume) can recreate the materialization.
- **Harness canonical path: `<work_dir>/harness/<sandbox_id>.ext4`**
  — same shape.
- Snapshot capture refuses non-conformant paths (fail-fast at the
  source rather than fail-mysteriously at restore time on a
  different host).

### Per-FC mount namespace for concurrent restores

A snapshot may be restored multiple times concurrently (N warm
slots from one template; a leased slot whose refill is in-flight).
Every restored FC opens the same embedded `path_on_host` from
state.bin. Sharing one file across N writable FCs corrupts the
rootfs.

The fix is `unshare(CLONE_NEWNS)` per FC process: each FC runs in
its own mount namespace where the canonical path bind-mounts to a
per-FC writable copy. The host's view keeps the canonical path
unchanged; the FC's view sees its own private file at the same
path.

Implementation: `Command::pre_exec` in the FC child runs the
namespace + bind-mount setup between fork and exec, before FC's
`main()` ever runs. The host process is unaffected.

This is the standard pattern in Lambda's microVM stack (NSDI '20),
FC's own jailer, runc, and kubelet — adopted here because the
"one canonical path, many private views" pattern is exactly what
multi-restore needs.

### Inflight-refill race guard (correctness invariant)

`WarmPool::maybe_refill` is called from every heartbeat-response
*and* from the 5s `gc_tick`. Without serialization per template,
two concurrent calls each saw `current=0 < target=1`, each spawned
its own `backend.restore` task, and the second collided with the
first on the source-sandbox-id-keyed vsock UDS path embedded in
`state.bin` → EADDRINUSE on every refill thereafter (caught by
dev-VM e2e, 92 failures in 7 minutes; hotfix `34b18aa`).

The guard is a per-template `DashMap<TemplateRef, ()>` claimed via
the atomic `Entry` API:

```rust
match self.inner.inflight_refills.entry(template_ref) {
    Entry::Occupied(_) => return,            // someone else is restoring
    Entry::Vacant(slot) => { slot.insert(()); }
}
```

The DashMap shard's write lock spans the entire match — the
`Vacant → insert` transition cannot race a parallel call's
`Vacant → insert`. The buggy alternative (`matches!(entry(), Vacant(_))`
followed by a separate `.insert()`) dropped the lock between the
check and the write and triggered the bug.

The spawn callback removes the flag *before* pushing to the free-
list so the next `gc_tick` can fire another refill if the slot was
consumed in the meantime — same correctness shape, opposite
direction.

### Bootstrap-after-restore: the unified rehydration path

Both milestones use the same wire choreography after FC `restore`:

1. Host re-creates TAP + vsock UDS at canonical paths.
2. Bootstrap (alive in the restored guest) is on `accept()`.
3. Host dials bootstrap on the UDS, reads `BOOTSTRAP_READY_BYTE` (0xEB).
4. Host pushes `BootstrapLaunch{argv, env, harness_dev,
   harness_mount}` (the last two are new under M1.12 — bootstrap
   mounts the harness device itself before exec; see "Harness
   late-bind via PATCH /drives" below).
5. Bootstrap mounts `<harness_dev>` at `<harness_mount>` (no-op on
   the M2 resume path where the harness is already mounted in the
   session-specific snapshot), reaps any prior agent child, and
   spawns the agent process.

Wire surface stays the same `engram-harness-proto` `BootstrapLaunch`
frame; it grows two optional fields. No new in-guest RPCs.

### Harness late-bind via PATCH /drives (M1.12, option D)

The original M1 design implicitly assumed the bake-time canonical
snapshot was captured *after* engram-init mounted the harness
substrate. That coupled each `templates` row to a specific
`(image, harness)` pair: N images × M harnesses = N×M bakes. With
the demo image + claude/codex/none harnesses the matrix is small,
but it scales with operator-added harnesses and forces a rebake on
every harness bump.

Option D decouples them. The bake-time capture point moves *earlier*
— to the moment bootstrap returns from `accept()` *before* the
harness device has been touched — and per-session harness selection
is done at warm-restore time via Firecracker's `PATCH /drives`.

Mechanics on the bake side:

- A stub harness ext4 (16 MiB empty file at the canonical path
  `<work_dir>/harness/stub.ext4`) is attached as the FC harness
  drive. It exists on every host as a static asset shipped with the
  host-agent binary; it never holds session data.
- engram-init mounts the rootfs only. It does **not** mount the
  harness substrate. (Same engram-init binary works on cold-create
  too — bootstrap, not init, is now the mount call site.)
- engram-init exec's bootstrap. Bootstrap accepts on vsock port
  1025. The snapshot is taken here.
- State.bin embeds the harness drive's `path_on_host` =
  `<work_dir>/harness/stub.ext4`. Memory.bin chunks contain kernel
  pages, bootstrap process pages, and the kernel page cache for
  rootfs only — no harness-specific bytes.

Mechanics on the warm-restore side:

1. `load_snapshot_paused` — restore memory + state.bin. UFFD handler
   set up. VM stays paused.
2. `patch_drive("harness", <session_harness_path>)` — FC swaps the
   stub for the session's chosen harness ext4. The kernel hasn't
   run yet, so there's nothing in its page cache referencing the
   stub.
3. `patch_vm_state(Resumed)` — FC kicks all virtio queues
   (`"Artificially kick devices"` in FC logs). The kernel virtio-
   blk driver re-reads device capacity. If the new file is a
   different size from the stub (it always is — real harness ext4s
   are MiBs–hundreds-of-MiBs), the kernel emits a
   `detected capacity change` uevent and **invalidates its buffer
   cache** for that block device. Even if the bake's profiling pass
   touched the stub (see M1.14 below), the resume path's cache is
   clean.
4. Bootstrap accept() returns when the host's vsock CONNECT lands.
5. Host writes `BootstrapLaunch{argv, env, harness_dev=/dev/vdb,
   harness_mount=/run/engram/harnesses/<name>}`.
6. Bootstrap does `mount(2)` on `/dev/vdb` at the requested mount
   point (bootstrap runs as PID 1, has CAP_SYS_ADMIN), then exec's
   the harness binary.

The mechanism is verified end-to-end on the dev VM by
`crates/engram-sandbox-firecracker/tests/patch_drive_swap.rs`
(commit `3c7cd8e`). Two variants pass: the production shape (no
guest read of vdb pre-snapshot) and a staleness variant (3 reads
pre-snapshot to pollute the page cache; cache is still correctly
invalidated post-PATCH+resume).

Implications:

- `templates` unique key drops from `(image_repo, image_tag,
  harness_pack_uri)` to `(image_repo, image_tag)`. Harness column
  becomes nullable or removed. One bake per image, period.
- Session-create's `resolve_template` no longer takes a
  harness_pack_uri; warm-lease works for any (image, harness)
  pair as long as the image's template exists.
- The cold-create path also moves the harness mount from
  engram-init to bootstrap. Cold path is unchanged otherwise.
- M2 resume and idle-thaw do **not** use PATCH /drives. They
  restore session-specific snapshots where the harness was
  already mounted at snapshot time; PATCH would invalidate the
  kernel's warm page cache for the harness, which would hurt M2
  more than help. This is a clean architectural split: PATCH is a
  warm-pool-only mechanism, session-specific snapshots restore
  embedded paths as-is.

### Working-set recording with synthetic profiling (M1.14)

The chunked-memory primitive (ADR 0007) makes restore lazy: chunks
are faulted in by UFFD on first kernel access. That's correct but
slow when chunks are GCS-cold — each fault is one GCS RTT
(~200-500 ms in-region), and 10-30 sequential faults during kernel
resume + bootstrap wake-up serialize into multi-second restores.

REAP (ASPLOS '21) showed that a one-time profiling pass that
records the working-set chunk order, then bulk-loads that set on
subsequent restores, gives 3.7× speedup on average. We adopt the
same primitive, with a twist for option D.

Bake-time profiling pass (after canonical capture, before
finalizing the template):

1. Restore the just-captured snapshot with the UFFD handler
   instrumented to record `(chunk_hash, fault_order_index)` for
   every page fault.
2. Let the VM run idle for ~3 s — long enough for the kernel
   scheduler to settle, bootstrap to re-enter accept(), virtio-blk
   queue notifiers to stabilize.
3. Then run a **synthetic post-restore profile**: from the host,
   dial bootstrap, write a synthetic `BootstrapLaunch{argv=/bin/
   true, env={}, harness_dev=/dev/vdb, harness_mount=/run/engram/
   harnesses/_profile}`. Bootstrap mounts the stub harness and
   exec's `/bin/true`. This forces the kernel to touch the pages
   that option D's warm-lease *will* touch: ext4 mount path
   (`ext4_fill_super`, `__ext4_iget`, etc.), execve path
   (`do_execveat_common`, `load_elf_binary`), and the relevant
   syscall trampolines.
4. Stop the VM. Serialize the recorded chunks (in fault order,
   deduplicated) into `working_set.json` with shape:

   ```json
   {
     "version": 1,
     "snapshot_id": "...",
     "chunks": [
       {"hash": "sha256:...", "first_fault_ms": 0},
       {"hash": "sha256:...", "first_fault_ms": 4},
       ...
     ],
     "total_pages": 2143,
     "synthetic_mount_exec": true
   }
   ```

5. Upload `working_set.json` alongside the sidecar JSON. Reference
   it from `SnapshotMetadata.working_set_blob_key`.

The synthetic-mount-exec step is the key adaptation to option D.
Without it, the bake-time idle profile would miss the kernel pages
that get touched only during the option-D warm-lease (mount + exec
fire only after the host writes the launch frame). Those pages
would be GCS-cold faults on every warm session, eroding the
prefetch win. With the synthetic step, the recorded working set
covers the full critical path from `patch_vm_state(Resumed)`
through harness execve — which is exactly what we want to be
NVMe-hot at warm-lease time.

The synthetic mount uses the stub harness (it's still attached at
bake time). The bytes it reads from the stub don't matter — only
the kernel pages exercised in the mount + execve syscalls do. The
stub's tiny ext4 keeps the profile fast and deterministic.

Restore-time consumption (see M1.13 below).

### Eager-prefetch refill + snapshot chunk cache (M1.13, M1.15)

`WarmPool::maybe_refill`'s spawned task currently does:

```
backend.restore(metadata)
  → spawn firecracker
  → materialize stub harness
  → load_snapshot_paused  // chunks loaded lazily by UFFD
  → push to free_lists
```

Under M1.13, the task fetches working-set chunks to NVMe *before*
the restore, then everything else *after* the slot is published:

```
backend.restore(metadata)
  → spawn firecracker
  → materialize stub harness
  → read working_set.json (if present in metadata)
  → parallel materialize working-set chunks to NVMe
        (8-wide concurrency, content-addressed paths)
  → load_snapshot_paused  // UFFD faults hit NVMe, not GCS
  → push to free_lists    // slot advertised in next heartbeat
  → tokio::spawn:
      → parallel materialize ALL remaining chunks in memory_manifest
        (background; doesn't block first lease)
```

Without `working_set.json` (e.g., older templates baked before
M1.14), the prefetch falls back to "fetch all chunks before
publish" — slower refill but correct. With it, refill drops from
"fetch 32 chunks of 16 MiB" (~2 s parallel from GCS) to "fetch ~5
chunks" (~300 ms parallel) before the slot is usable.

The cache itself (M1.15) promotes today's ad-hoc per-chunk file
writes into a `SnapshotChunkCache` component:

- Content-addressed NVMe storage under
  `<work_dir>/chunks/sha256/<aa>/<bbbb…>` (mirrors the existing
  chunk-store layout).
- Pinning hints: chunks named in any active template's
  `working_set.json` are excluded from LRU eviction.
- Bounded by a `--chunk-cache-budget-bytes` flag (default 50 GiB);
  oldest non-pinned chunks evicted when budget exceeded.
- Metrics: `engram_chunk_cache_size_bytes`,
  `engram_chunk_cache_hits_total{cache_tier="nvme|blobstorage"}`,
  `engram_chunk_cache_evictions_total{reason="lru|pinned_changed"}`.

The cache is the L1 layer in the same architectural shape as AWS
Lambda SnapStart's tiered cache (their L1=NVMe, L2=distributed,
L3=S3). Our L2 is BlobStorage directly; we don't plan a
distributed mid-tier in v1 — measured GCS in-region latency is
acceptable as the miss path.

### Per-VM network namespaces for warm slots (M1.16)

M1.0–M1.15 leave warm-restored VMs **netless**: the bake-time FC
config sets `net_pool = None`, so the snapshot has no virtio-net
device and the kernel cmdline has no `ip=…`. The cold path stays
correct (per-session TAP + `/30` + `ip=…` baked into cmdline at
create time), but warm sessions can't reach the egress proxy and
the dashboard SHELL tab can't dial `ttyd` over TCP. The fix can't
be "bake an `ip=…` into the snapshot then restore" because FC's
`PATCH /network-interfaces/{iface_id}` doesn't permit changing
`host_dev_name` post-load (verified against the v1.10.1 swagger),
so every restore would be forced to recreate the *same* TAP name
on the host — N>1 warm slots on one host would collide. Even with
N=1 per template, every template's bake picks `10.200.0.0/30`
(allocator starts at zero), so two templates on one host would
collide too.

M1.16 puts every warm-restored VM in its own network namespace.

```
┌──────────────────────────────── host root netns ───────────────────────────┐
│                                                                            │
│         ┌─ engr-br0 (bridge, no IP) ────────────────────────────┐          │
│         │      ▲                ▲                ▲              │          │
│   veth-A-vm1   veth-A-vm2  veth-A-vmN          (uplink)         │          │
│         │      │                │                ▲              │          │
│         ▼      ▼                ▼            iptables           │          │
│   ┌─ vm1 ns ─┐ ┌─ vm2 ns ─┐ ┌─ vmN ns ─┐    REDIRECT→9443       │          │
│   │ veth-B  │ │ veth-B  │ │ veth-B  │      MASQUERADE          │          │
│   │ + TAP   │ │ + TAP   │ │ + TAP   │      egress-proxy:9443   │          │
│   │   ↕     │ │   ↕     │ │   ↕     │      DNS-proxy:5353      │          │
│   │  eth0=  │ │  eth0=  │ │  eth0=  │                          │          │
│   │ 10.200. │ │ 10.200. │ │ 10.200. │                          │          │
│   │ 0.2     │ │ 0.2     │ │ 0.2     │                          │          │
│   └─────────┘ └─────────┘ └─────────┘                          │          │
│       FC          FC          FC                                           │
└────────────────────────────────────────────────────────────────────────────┘
```

Per-VM layout (`engr-vm-<sandbox_id_prefix>`):

- `ip netns add engr-vm-<id>`
- `ip link add veth-A-<id> type veth peer name veth-B-<id>`
- `ip link set veth-B-<id> netns engr-vm-<id>`
- `ip link set veth-A-<id> master engr-br0 ; ip link set veth-A-<id> up`
- Inside the netns:
  - bring up `lo`
  - `ip tuntap add tap-engr-<bake_id> mode tap` — the bake's TAP
    name; reusing it inside this netns avoids the global-namespace
    collision FC's `host_dev_name` invariant would otherwise force
  - assign `10.200.0.1/30` to the TAP (matches the VM's baked-in
    default gateway from the bake-time `ip=…` cmdline)
  - default route via `veth-B-<id>`'s peer endpoint

The egress proxy + DNS proxy keep listening on host-root
`127.0.0.1:9443` / `:5353`. iptables PREROUTING REDIRECT runs on
the bridge ingress (`-i engr-br0`), which captures every netns's
egress traffic in one ruleset. Because every VM's `eth0` is
`10.200.0.2`, the proxy registry would collide on `guest_ip` —
we SNAT each netns's outbound source from `10.200.0.2` to a
**unique-per-VM host-pool IP** allocated by the existing
`NetworkAllocator`. The proxy registry stays keyed on that
unique IP; conntrack reverses the SNAT on return packets.

In effect: from inside the VM, networking looks the same on cold
and warm paths (`eth0 = 10.200.0.2`, gateway `10.200.0.1`). From
the host's view, every VM has a unique routable IP — same model
the egress proxy already enforces against. The only new mechanism
is the per-netns indirection and the SNAT mapping table.

**Bake side**: enable `net_pool` at bake time so the snapshot has
a virtio-net device + `ip=…` cmdline. Pre-M1.16 bakes (no
virtio-net in the snapshot) stay restorable; they boot netless
just like today — no regression. Bake host needs `CAP_NET_ADMIN`
(workflow grants it via `setcap` on `engram-cli`).

**FC spawn**: the host-agent enters the netns via
`setns(CLONE_NEWNET)` immediately before exec'ing firecracker;
the FC process and any UFFD/uffd-handler children inherit it.
`spawn_firecracker` grows a `netns_path: Option<&Path>`
argument. Cold path passes `None` (legacy direct-TAP-on-root
shape) until a future ADR unifies the two; warm path always
passes `Some(/var/run/netns/engr-vm-<id>)`.

**Latency**: per-VM netns provisioning adds ~30–60 ms wall-clock
to refill (kernel `ip netns add` + veth pair + bridge attach +
`ip tuntap`); refill is async to the user. Steady-state per-packet
overhead is one extra L2 hop (veth → bridge), ~10–50 µs, dwarfed
by the existing FC virtio-net path.

**Teardown**: `destroy()` deletes the netns (auto-cleans veth +
TAP inside), removes veth-A from the bridge, releases the SNAT
slot. Host-side rules are static (one ruleset on the bridge);
no per-VM iptables to GC.

**Lifecycle of the bridge**: host-agent startup brings up
`engr-br0` once (idempotent, like `host_startup`'s iptables
ruleset); it's the single host-wide L2 fabric all warm-VM netns
veth-pairs plug into.

### Cost claim: canonical-mmap page-cache sharing

The UFFD handler mmaps the canonical memory.bin with `MAP_PRIVATE`
(`crates/engram-uffd-handler/src/chunked.rs:14-22`). N concurrent
sandboxes of the same template each get their own mmap of the same
file. The kernel page cache de-duplicates read pages across the
mappings — N warm slots × 2 GiB of memory does **not** cost N × 2 GiB
resident.

This is load-bearing for warm-pool depth economics. It is also
**untested at scale** today — no test exercises multi-restore-from-one-
canonical. M1 ships a bench (`warm_pool_memory`) that boots N=20
sandboxes and validates `MemAvailable` vs the sum of per-cgroup
`memory.current`. If they diverge, cgroup config tweaks
(`memory.swap.max=0`) or a conservative cap on warm-pool depth gate
production rollout. KSM is the documented fallback if shared
accounting can't be made to work; currently rejected for side-channel
reasons.

## Milestone 1 — Warm pool

Goals:

- p50 warm-lease session create ≤ 150 ms; p99 ≤ 250 ms.
- p95 warm-pool refill ≤ 2 s on a chunk-cache-cold host (this is
  the new measurable that M1.13+M1.14 produce; today's refill on
  a fresh host pays multi-second GCS chunk-fault tax).
- p95 first warm session on a freshly-deployed host ≤ 1 s (today
  this is "1b: ~3 s NVMe-cold faults during resume"; with prefetch
  it collapses to scenario 3).
- v1 warm-pool depth: N=1 per template per host (mount-namespace
  isolation makes N>1 correctness-safe; the depth-cap is a memory-
  cost decision, see warm_pool_memory bench result).
- One template row per `(image_repo, image_tag)` — harness is
  late-bound per session via the option-D PATCH path. Eliminates
  the N×M bake-matrix coupling.
- Lease failure (stale template, host gone, capacity exhausted) falls
  through to cold-create with one tracing::warn.
- Eager hydration on image upload: enabling an image in the
  dashboard's `Settings → Images` panel (POST /api/enabled-images)
  cascades — when bundle.json carries a `canonical_snapshot`, coord
  inserts `snapshots` + `templates` rows in one PG transaction. The
  next heartbeat-ack ships the new template_ref, hosts begin filling
  warm slots, and the first user session lands on the warm path.
  Memory chunks are already in BlobStorage from the bake pipeline,
  so first-touch is a GCS pull (intra-VPC, fast) not an OCI registry
  pull. Eliminates the "first user pays the OCI pull cost on a
  freshly-uploaded image" UX gap.

Components:

- **Image builder extension**: `CanonicalCaptureConfig` already
  snapshots memory at bake. Add state.bin + sidecar upload after the
  capture. Under M1.12, snapshot point moves from "post-engram-init
  full mount" to "bootstrap on `accept()` with stub harness attached
  but unmounted." Under M1.14, the bake pipeline grows a synthetic
  profiling pass that produces `working_set.json` alongside the
  sidecar.
- **`templates` table**: maps `(image_repo, image_tag) →
  snapshot_id + vcpus + memory_mib`. Rebake flips prior row
  `active=false`. (M1.12 drops `harness_pack_uri` from the unique
  key; was redundant once harness binding is per-session.)
- **Image-enablement cascade** (M1.11, in
  `crates/engram-coordinator/src/api/enabled_images.rs`): after
  validating the bundle, if `bundle.canonical_snapshot.is_some()`,
  insert into `snapshots` (with `recoverable=true`) and
  `upsert_template` in one PG transaction. Idempotent on re-
  enable. Replaces the deferred admin endpoint discussed in earlier
  drafts.
- **Per-host `WarmPool`** (existing
  `crates/engram-host-agent/src/warm_pool.rs`):
  free-list per template_ref; refill loop; 60 s grace on rebake.
  v1 effective N(T)=1 default per host. The autoscaler computes
  a target from observed lease rate but **CEILING_TARGET=1** in
  v1: N>1 concurrent restores from one snapshot collide on the
  source-sandbox-id-keyed vsock UDS path (FC's state.bin embeds
  it, two FCs can't bind the same Unix socket). The per-FC
  mount-namespace + bind-mount approach above unblocks N>1; once
  that lands, raise the ceiling. M1.10's `multi_restore` test is
  serial (the warm-pool refill semantic) and passes today;
  `warm_pool_memory` is scaffolded for the un-block PR.
- **Working-set recorder** (M1.14, new in image-builder): bake-side
  profiling pass that produces `working_set.json`. Synthetic
  mount+exec step exercises the kernel pages option-D warm-lease
  touches, not just the post-restore idle path. See "Working-set
  recording with synthetic profiling" above.
- **Refill prefetch** (M1.13, in `WarmPool::maybe_refill`'s spawn):
  parallel-fetch working-set chunks before `load_snapshot_paused`;
  background-fetch remaining chunks after `push to free_lists`.
  Fallback to "fetch all before publish" when `working_set.json` is
  absent (older templates).
- **`SnapshotChunkCache`** (M1.15, new on the host-agent): first-
  class NVMe cache for snapshot chunks. Pinning hints from active
  templates' working sets; LRU eviction with a configurable budget;
  exported metrics. The structural analog of AWS Lambda SnapStart's
  L1 cache tier.
- **Bootstrap mount + harness late-bind** (M1.12, in
  `engram-bootstrap`): bootstrap learns to `mount(2)` the harness
  device at the path the host requests in `BootstrapLaunch`, before
  exec'ing the harness binary. engram-init stops mounting the
  harness — bootstrap owns it on both warm and cold paths.
- **gRPC additions**: `LeaseWarmSandbox`, `LaunchWarmSandbox`,
  `ListWarmSlots`. `LeaseWarmResponse` is a oneof of
  `{sandbox_id, StaleTemplate{current_ref}, no_capacity}` so the
  scheduler can distinguish "wrong template ref" from "pool empty".
- **Scheduler change**: `pick_for_session` resolves template_ref. If
  warm-eligible, parallel-ask top-K candidate hosts (by capacity +
  heartbeat-reported warm slots). First non-stale non-empty lease
  wins. `StaleTemplate` errors lazily update coord's known set. All
  no → existing cold-create.
- **Heartbeat extension**: `HostCapacityReport.warm_slots:
  HashMap<TemplateRef, u32>` — coord skips parallel-ask for zero-slot
  hosts. Autoscaler driven by observed lease-success-rate, not
  heartbeat-reported slots (skew-resilient).
- **Autoscaler**: per-template lease/min over a 5-min window;
  N(T) = max(2, ceil(lease_rate × refill_time × 1.2)); floor=0 if 0
  leases in 30 min AND template inactive.
- **Per-VM network namespaces** (M1.16, new in
  `crates/engram-sandbox-firecracker/src/net.rs`): warm-restored
  VMs run inside `engr-vm-<id>` netns'es, plugged into a
  host-wide `engr-br0` bridge via veth pair, with SNAT to a
  unique-per-VM host-pool IP. Lets every warm slot have working
  egress (through the existing proxy) and a routable
  `guest_ip` for the dashboard SHELL tab — without the FC
  `host_dev_name` PATCH that the API doesn't support. Bake
  enables `net_pool` so the snapshot carries a virtio-net device
  + `ip=…` cmdline. Cold path stays direct-TAP-on-root in v1;
  unification deferred to a follow-up. See "Per-VM network
  namespaces for warm slots (M1.16)" above.

## Milestone 2 — Durability

Goals:

- Sessions in `Active` or `Idle` state survive host loss (graceful
  drain OR ungraceful crash).
- Resume p95 ≤ 1 s (warm-pool-eligible) / ≤ 3 s (cold-restart).
- Lost work bounded by snapshot frequency — default: idle-entry-
  triggered, plus a 5-min wall-clock ceiling.
- Conversation history (PG-backed `session_events`) already survives;
  M2 covers workspace + agent state.

Components:

- **`SessionStatus::Paused`**: snapshot exists, sandbox destroyed,
  system-driven. User clicks Resume; system auto-retries up to N
  times before flipping to Failed.
- **Schema**: `sessions.snapshot_ref UUID`, `snapshots.state_blob_key`,
  `snapshots.sidecar_blob_key`, `snapshots.rootfs_blob_key` (nullable —
  interim writable-rootfs upload), `snapshots.template_ref`.
- **Writable-rootfs interim upload**: `disk_manifest: None` everywhere
  today despite NBD chunked-disk machinery existing (`crates/engram-
  sandbox-firecracker/src/lib.rs:1943, 2383, 2487`). Without uploading
  the writable rootfs, cross-host Resume silently loses edits. M2 v1
  tars + zstd-compresses the writable rootfs and uploads as opaque
  blob. Chunked-disk replacement is a follow-up ADR when the writable-
  NBD plumbing lands.
- **Background snapshot uploader**: per Active sandbox, fires on
  harness-idle entry (existing idle_evictor trigger) AND a 5-min wall-
  clock ceiling. Idle eviction's existing snapshot path becomes the
  durability path (don't destroy after upload). Idempotent: skips if
  no chunk delta since the last `recoverable=true` snapshot.
- **Graceful drain enhancement**: extends `crates/engram-host-agent/
  src/shutdown.rs`. Bump `--shutdown-deadline-secs` default 90 → 180 s
  when graceful-snapshot-upload is enabled. Upload concurrency 4
  (separately bounded from snapshot fan-out 8). Final step: POST
  `/api/hosts/:id/draining` with `(session_id, snapshot_id)` tuples so
  coord can pre-stage Paused transitions if the host dies before its
  terminal log line.
- **Dead-host reaper extension** (`dead_host.rs`): on host death, for
  each Active session, if `snapshot_ref.recoverable=true` → Paused +
  null host_id + null sandbox_id; else Failed (current behavior).
- **Resume API**: `POST /api/sessions/:id/resume`. Validates Paused or
  Idle + recoverable snapshot. Tries warm-pool-eligible templates
  first; for sessions with rootfs delta, cold-restart via
  `host.restore(snapshot_metadata)`. Fresh BootstrapLaunch on restore.
- **SPA UX**: Paused renders with "Last active <duration> ago" + Resume
  button.

## Files modified (overview)

M1.0–M1.10 (landed):

- `crates/engram-sandbox-firecracker/`: path canonicalization in
  snapshot/restore; new `client::patch_drive` +
  `load_snapshot_paused` primitives (commit `3c7cd8e`).
- `crates/engram-host-agent/`: new `warm_pool.rs`; metrics module
  with `kind=cold/warm` split; gRPC server emits agent_handshake
  histogram on cold path.
- `crates/engram-coordinator/`: new templates resolver + scheduler
  warm-lease path in `host_registry.rs`; metrics module;
  `CreateSessionResponse.kind` field.
- `crates/engram-image-builder/`: post-canonical-capture state.bin +
  sidecar upload; bundle.json v3 carries canonical_snapshot.
- `crates/engram-protocol/`: proto additions for warm-pool RPCs;
  HostCapacityReport.warm_slots.
- `crates/engram-core/`: SessionStatus shape; trait surface for
  LeaseWarm/LaunchWarm; SnapshotMetadata schema.
- `deploy/migrations/`: 0025 (templates).

M1.11–M1.15 (queued):

- **M1.11** edits `crates/engram-coordinator/src/api/enabled_images.rs`
  to cascade into snapshots + templates inserts when bundle.json
  carries `canonical_snapshot`. Migration may drop `harness_pack_uri`
  from `templates` unique key (or hold to M1.12 if we want to keep
  the column for a release).
- **M1.12** spans `engram-init` (drop harness mount), `engram-bootstrap`
  (gain mount(2)), `crates/engram-image-builder/` (snapshot point
  moves to bootstrap-on-accept with stub harness attached),
  `crates/engram-sandbox-firecracker/src/lib.rs` (`restore_in_jail`
  switches to `load_snapshot_paused` + `patch_drive` + explicit
  resume; ship the stub harness ext4 as a static asset under
  `<work_dir>/harness/stub.ext4`), `crates/engram-host-agent/src/
  warm_pool.rs` (`launch` adds `patch_drive` call). Templates
  schema migration drops `harness_pack_uri` from the unique key.
- **M1.13** edits `crates/engram-host-agent/src/warm_pool.rs`
  (refill spawn block grows pre-publish parallel fetch + post-
  publish background fetch). Touches `crates/engram-host-agent/
  src/pooled_backend.rs` for the materialize-to-NVMe helper.
- **M1.14** adds a profiling pass to `crates/engram-image-builder/
  src/lib.rs` (synthetic mount + execve via a temporary
  BootstrapLaunch over vsock to the just-snapshotted VM). Adds
  `working_set_blob_key` to `SnapshotMetadata`. M1.13's prefetch
  starts consuming it.
- **M1.15** adds `crates/engram-host-agent/src/snapshot_chunk_cache.rs`
  (LRU + pinning + budget + metrics). Refactors `pooled_backend.rs`'s
  ad-hoc chunk writes through it.

M2 (still future):

- `crates/engram-host-agent/`: new `snapshot_uploader.rs`, edits to
  `shutdown.rs` for upload pipeline + extended drain deadline, edits
  to `idle_evictor.rs` for durability path.
- `crates/engram-coordinator/`: new `api/resume.rs`, edits to
  `dead_host.rs` for Paused transition.
- `crates/engram-core/`: SessionStatus::Paused.
- `deploy/migrations/`: 0026 (sessions.snapshot_ref), 0027 (snapshots
  portable columns).

## Implementation order (M1.11 → M1.15)

Each phase is intended to land as its own commit + measurable on
the same prod dashboard. Ordering matters: M1.11 is the unblocker
(without it the warm pool never fires in prod); M1.12 changes the
critical-path mechanism; M1.13 + M1.14 are the latency wins that
make warm-lease actually sub-1 s; M1.15 is the polish that turns
the chunk cache into a tunable, observable component.

1. **M1.11 — image enablement → templates cascade.** ~150 LoC in
   `enabled_images.rs` + the existing PG transaction helpers. Lands
   in this repo (engrams). Measurable: first
   `engram_sandbox_boot_seconds{kind="warm"}` series with non-zero
   counts on the prod dashboard.

2. **M1.12 — option D.** ~500 LoC across engram-init,
   engram-bootstrap, image-builder, host-agent, coord. The
   `tests/patch_drive_swap.rs` integration test (commit `3c7cd8e`)
   becomes the regression gate. Migration drops `harness_pack_uri`
   from `templates`'s unique key. Measurable:
   `engram_session_boot_seconds{kind="warm",phase="total"}` p50
   drops to ~700 ms — sub-1 s achieved on warm-cache hosts.

3. **M1.13 — eager prefetch refill.** ~50 LoC in `WarmPool::
   maybe_refill`. Measurable: `engram_warm_pool_refill_seconds` p95
   on a chunk-cache-cold host drops from ~5 s (UFFD-lazy GCS faults)
   to ~2 s (parallel pre-publish GCS pulls).

4. **M1.14 — working-set recording + replay with synthetic
   profiling.** ~400 LoC across image-builder (profiling pass +
   stub-harness mount + execve drive over vsock + working_set.json
   writer) and host-agent (consume the manifest, narrow the pre-
   publish fetch). New `working_set_blob_key` field on
   `SnapshotMetadata`. Measurable: refill p95 drops from ~2 s
   (eager-everything) to ~500 ms (working-set-only); first warm
   session post-deploy is NVMe-hot from the first kernel fault.

5. **M1.15 — `snapshot_chunk_cache`.** ~200 LoC. Pinning, LRU,
   budget, metrics. Measurable: `engram_chunk_cache_hits_total`
   ratio visible on the dashboard; operators can tune the budget.

Implementation-order ADR change is purely additive — each phase is
behind the existence of `working_set.json` / `kind=warm` / etc., so
older deploys continue to function (cold-create everywhere, no
warm-lease).

## Risks

1. **Path canonicalization breaks legacy in-flight snapshots.** Any
   sandbox running today has a non-conformant jail path; its in-memory
   snapshot would fail validation. M1 only produces canonical paths
   going forward; pre-M1 snapshots stay restorable on the same host
   (legacy path). Drain semantics in `dead_host.rs` already mark them
   Failed on host loss, which is the existing behavior — no
   regression.
2. **Canonical-mmap cgroup accounting unvalidated at scale.** Mitigated
   by gating bench (`warm_pool_memory`) before prod rollout. If
   diverged, fallback is conservative depth cap + documented in the
   ADR.
3. **Heartbeat / template-ref skew under load.** `LeaseWarmSandbox`
   returns typed `StaleTemplate{current_ref}` so scheduler distinguishes
   stale-pool-host from no-capacity-host. Hosts keep old-ref slots alive
   60 s after a new ref appears.
4. **Snapshot during active agent I/O.** Substrate-only semantics
   forbids it. Background snapshots only fire on idle-entry. Drain-
   time snapshots accept the I/O-disruption cost because the
   alternative (lose all work) is worse.
5. **Resume vs. fresh-start UX honesty.** Document the contract: Resume
   restarts the agent against persisted state; no live freeze-thaw
   promise. SPA copy reflects this.
6. **Full snapshots only.** FC `client.rs:162` hardcodes
   `SnapshotType::Full`; M2's 5-min wall-clock ceiling rewrites full
   memory.bin every time. Diff snapshots are a follow-up ADR if
   measured bandwidth becomes a problem.
7. **MIG drain deadline tightness.** Existing 90 s budget + new
   per-sandbox upload (~6 s memory + ~3-5 s rootfs at 4× concurrency)
   → ~75 s p99 for a 10-sandbox host. 180 s deadline gives 2× headroom.
8. **(M1.16) Two parallel networking models on one host.** Cold
   path stays direct-TAP-on-root (per-VM /30 from the host pool);
   warm path uses netns + bridge + SNAT. Mitigated by sharing the
   same `NetworkAllocator` (both paths draw from `10.200.0.0/24`,
   so a slot leased by either is invisible to the other) and the
   same egress-proxy registry keyed on the host-visible IP.
   Future ADR unifies the two when there's a real reason to. See
   "Cold-path → netns unification" under Open questions.
9. **(M1.16) Concurrent warm restores still collide on FC's
   string state.** `state.bin` embeds the vsock UDS path as a
   filesystem string; two concurrent restores from one snapshot
   both `bind()` that path and the second loses with `EADDRINUSE`.
   That's why `CEILING_TARGET=1` in `warm_pool.rs` and why
   `multi_restore` is a serial test. M1.16's network namespace
   only solves the network-side collision; mount namespaces are
   needed to also isolate filesystem paths. Same kernel-namespace
   pattern, same shape of host-agent change. Deferred as long as
   `target=1` per template per host satisfies measured demand —
   see "Per-FC mount namespace for concurrent restores" under
   Open questions.

## Verification

M1 acceptance gates:

- `cargo test -p engram-coordinator --test warm_pool_integration` —
  fake coord + 2 host-agents; warm pool fills to N=2; 100-session p99
  ≤ 250 ms.
- `cargo test -p engram-sandbox-firecracker --test multi_restore --
  --ignored --nocapture` (dev-vm) — N=5 restores from one canonical;
  independent guest_ip; no cross-talk.
- `cargo test -p engram-sandbox-firecracker --test patch_drive_swap
  -- --ignored --nocapture` (dev-vm, M1.12) — option-D mechanism
  test: load_snapshot_paused + patch_drive + resume; guest sees new
  bytes on next read. Both the production-shape and staleness
  variants pass (commit `3c7cd8e` already in tree).
- `cargo bench -p engram-host-agent --bench warm_pool_memory` (dev-vm)
  — N=20 sandboxes; MemAvailable within 10% of canonical-shared total.
- New (M1.13/M1.14): refill-latency benchmark scraping
  `engram_warm_pool_refill_seconds`. Target p95 ≤ 2 s on a chunk-
  cache-cold host with `working_set.json` present. Compare against
  no-prefetch baseline.
- New (M1.12): `engram_session_boot_seconds{kind="warm",phase="total"}`
  p95 ≤ 1 s on the dev VM after warm pool is hot. Today's
  `kind="cold"` p95 ~25 s is the baseline this is measured against.
- New (M1.16): `cargo test -p engram-sandbox-firecracker --test
  netns_warm_multi_slot -- --ignored --nocapture` (CI Linux+KVM
  job, runs as root). Provisions two warm slots from one
  canonical snapshot; asserts each has a distinct host-side
  reachable IP, each VM's eth0 boots to `10.200.0.2`, both can
  reach a host-loopback fake-upstream concurrently, and traffic
  doesn't cross between netns'es. Same test exercises teardown:
  every netns + veth + bridge port gone after `destroy()`.
- Manual rollout: engrams-internal with warm-pool depth N=1; 24 h soak;
  bump to N=2.

**Dev-VM e2e validation (2026-05-16):**

- Built `seed_warm_template` (commit `a803fda`) to register a real
  template + snapshot artifacts via the production code path:
  `FirecrackerBackend` → `PooledBackend` → `LocalBlobStorage`.
- Ran `mode=coordinator` (separate coord + host-agent processes,
  HTTP + gRPC between them) against docker-compose Postgres.
- Confirmed: refill #1 succeeded in <60 ms; 35+ s of heartbeat +
  gc_tick probes after that all observed `current=1 >= target=1`
  and skipped the spawn; zero EADDRINUSE failures.
- Confirmed: `POST /sessions` with `harness: {kind: "none"}`
  resolved the template, picked the host (via heartbeat-reported
  `warm_slots`), and fired `LaunchWarmSandbox` over gRPC.
- Surfaced two e2e-only bugs not caught by unit tests:
  - `34b18aa` — `matches!(entry, Vacant)` race in `maybe_refill`
    (see "Inflight-refill race guard" above).
  - `e96842d` — `harness_pack_uri: None` short-circuited the
    warm-lease block instead of using the `"none"` sentinel.
- Did *not* exercise the final agent-exec step: the seed binary
  uses the public Ubuntu rootfs (no `engram-bootstrap` baked in),
  so the warm-launch's vsock CONNECT to port 1025 returns EOF.
  Production rootfs (image-builder output) bakes bootstrap.

M2 acceptance gates:

- `cargo test -p engram-coordinator --test resume_integration` — host
  A dies mid-session; session → Paused; Resume restores on host B with
  byte-identical workspace.
- `cargo test -p engram-host-agent --test graceful_drain_upload` (dev-vm)
  — 5 sandboxes; SIGTERM; all 5 upload + Paused-eligible within 180 s.
- Manual rollout: engrams-internal force-roll MIG; all active sessions
  → Paused (not Failed); Resume restores user workspace files.

## Resolved design decisions (previously open)

- **Bake → `templates` row glue.** Resolved as M1.11: cascade in
  the existing `POST /api/enabled-images` handler. When the
  bundle.json validated during image enablement carries a
  `canonical_snapshot`, coord inserts `snapshots` + `templates`
  rows in one PG transaction. Idempotent on re-enable. No new
  admin endpoint, no image-builder-side network call, no GHA
  changes needed. The `seed_warm_template` dev binary becomes a
  legacy tool for dev VMs that don't run the full coord stack.
- **Harness coupling: per-harness bakes vs. harness-agnostic
  templates.** Resolved in favor of harness-agnostic (option D,
  M1.12). One template per `(image_repo, image_tag)`; harness
  binding happens at warm-restore via `PATCH /drives`. Rationale:
  (a) eliminates N×M bake matrix, (b) snapshot point moves
  earlier so the memory.bin chunks don't carry harness-specific
  bytes — full chunked-memory dedup across sessions of different
  harness, (c) the FC mechanism is verified by
  `tests/patch_drive_swap.rs`. Cost: bootstrap learns one
  `mount(2)` call; engram-init loses its harness-mount step.
- **Working-set replay vs. eager-everything prefetch on refill.**
  Resolved as both, layered. M1.13 ships eager-everything (no
  bake-time profiling needed; loads all chunks before publish).
  M1.14 layers working-set replay on top (smaller pre-publish
  fetch + background load of the rest). Falls back to M1.13's
  shape when a template predates M1.14's `working_set.json`.
- **Synthetic profiling vs. static kernel-page nomination.**
  Resolved in favor of synthetic profiling (M1.14): the bake-time
  profiling pass exercises mount(2) + execve(2) on a stub harness
  to capture the kernel pages that option-D's warm-lease will
  touch. Static nomination (hand-listing kernel symbols whose
  pages must be in the working set) is fragile and bound to kernel
  versions; synthetic profiling generalizes to future "session-
  side first actions" (chdir, workspace mount, etc.) without
  manual maintenance.

## Open questions / deferred

- **Diff snapshots** if M2 background-upload bandwidth becomes a
  problem.
- **Chunked writable disk** replaces M2's interim tar+zstd upload when
  the writable-NBD plumbing lands.
- **Workspace late-bind for git templates** via virtio-fs lets git
  templates also benefit from warm pool. Future ADR.
- **KSM as fallback** if cgroup-accounting validation forces it. Currently
  rejected for side-channel reasons.
- **Egress policy re-bind on cross-host Resume** — confirm
  `engram-egress-proxy` re-attaches when a sandbox restores on a host
  with a fresh TAP. Trace in the cross-host restore test.
- **Distributed L2 chunk cache.** AWS Lambda SnapStart runs an
  intermediate distributed cache between local NVMe (L1) and S3
  (L3). We skip this in M1 — measured GCS in-region latency is
  acceptable as the miss path when working-set prefetch is doing
  its job. Revisit if fleet-wide chunk-cache-miss rate climbs.
- **Per-FC mount namespace for concurrent restores (post-M1.16).**
  M1.16 puts each warm-restored VM in its own network namespace,
  which isolates the TAP name + per-VM IP. The remaining
  collision is FC's vsock UDS path: `state.bin` embeds it as a
  filesystem string keyed on the bake's sandbox_id, so two
  concurrent restores from one snapshot both `bind()` the same
  Unix socket on the host filesystem and the second fails with
  `EADDRINUSE`. Fix is the same kernel-namespace trick applied to
  a different namespace: `unshare(CLONE_NEWNS)` before
  `spawn_firecracker`, `mount --bind /per-vm-dir/<id>.vsock
  /var/lib/engram/<bake-id>.vsock` inside the namespace so FC's
  embedded path resolves to a per-VM inode. Same overall LOC
  budget as M1.16 (~200-300). Blocked behind raising
  `CEILING_TARGET` past 1 — until measured demand asks for
  N concurrent slots per template per host, sequential refill
  with N=1 covers it (a session destroying its slot releases the
  UDS before refill's next restore takes it).
- **Cold-path → netns unification (post-M1.16).** M1.16 left
  cold-create using the direct-TAP-on-host-root model and only
  put warm-restored VMs in netns'es. Two networking shapes on
  one host is the cost. Unifying — putting cold creates in netns
  too — would shrink `net.rs` to one provisioning path, simplify
  the test surface, and pre-stage cold for any future mount-ns
  work. Deferred as plumbing rather than design: same primitives
  (`provision_netns`/`teardown_netns`) already exist; main work
  is rewriting `create_in_jail` to thread the netns through and
  updating cold-path tests. ~300 LOC.

ADR 0012 ("warm pool deferred") and ADR 0009 ("graceful preemption
deferred") both move from `deferred` → `landed via 0014` after M2.

## Known issues observed in prod (post-M1.16)

Captured during the 2026-05-19/20 M1.16 rollout investigation
(session `b511cf9b-7356-44fc-b6b4-be859bf864d4`, disk-fill on
`engrams-fc-xngk`, Ops Agent enablement). Each item is a concrete
follow-up; ordering reflects rough priority (highest first).

1. **Idle-evict orphan snapshot dirs (caller-layer leak).**
   Commit `752aea3` (fix #1) wraps `PooledBackend::snapshot`'s
   post-FC chunk/upload work in cleanup hygiene — if chunking or
   upload fails, the FC dir gets `rm -rf`'d. But the *caller*
   layer (the idle-evictor's "snapshot → register → destroy"
   sequence) has no equivalent. If a step after the snapshot
   succeeds (PG insert / mark-idle / destroy / coord ack)
   fails, the snapshot dir stays on disk. Each retry mints a
   fresh `SnapshotId` and lays down another 4 GiB at
   `/var/lib/engram/sandboxes/snapshots/<new-uuid>/`. On session
   b511cf9b this leaked ~25 dirs × 4 GiB in 13 min, filling the
   99 GB disk on `engrams-fc-xngk`. Minimum fix: extend the
   cleanup hygiene to the idle-evict caller's failure path.
   Cleaner fix: see #2.

2. **`SnapshotId::new()` per idle-evict retry is the wrong
   default.** A `SnapshotId` represents an immutable artifact's
   identity — right for explicit "save this session" semantics,
   wrong for idle-evict's retry-until-success shape. Each retry
   creating a fresh ID means no two attempts share a path, so a
   prior attempt's leftovers can't be naturally overwritten —
   they orphan. Proper fix: idle-evict writes to a
   sandbox-keyed scratch path
   (`<work_dir>/sandboxes/snapshots/idle-scratch/<sandbox_id>/`);
   each retry overwrites in place; atomic-rename to the
   `SnapshotId::new()` final path only on full eviction
   success. Same shape as tempfile crates' "write tmp/, rename
   on success" pattern. Closes the orphan class entirely
   instead of papering over with cleanup hygiene. Subsumes #1.

3. **Stale `templates` rows pointing at missing snapshot
   blobs.** Surfaced immediately after the M1.16 Ops Agent
   commit made host-agent logs queryable in Cloud Logging:
   ```
   warm_pool refill failed ... read manifest: No such file or directory
   prefetch get_manifest <id>@v1: blob storage: blob not found
   download state.bin from snapshots/<id>/state.bin: blob not found
   ```
   Multiple `templates` rows in PG reference snapshot IDs whose
   bytes are absent from BlobStorage. Likely origin: earlier
   bake-cascade work (M1.11) where the demo image was enabled
   but `canonical_snapshot` block in `bundle.json` referenced
   blob keys that weren't actually populated, or where the
   blob upload failed silently. Hosts dutifully try to refill
   warm slots for each, hit blob-not-found, retry forever.
   Net effect: warm pool stays empty → every session falls
   through to cold-create (the 29.5 s "warm" activation on
   b511cf9b was actually cold). Two fixes needed:
   (a) `POST /api/enabled-images` cascade must verify
   `canonical_snapshot`'s blob keys are resolvable in
   BlobStorage **before** inserting the `templates` row.
   (b) A coord-side sweeper that prunes `templates` rows
   whose snapshot blobs go missing (or marks them inactive),
   so a one-time blob-store hiccup doesn't permanently brick
   warm pool for that template.

4. **No disk-pressure floor on idle-evict.** Even with #1 + #2
   fixed, a snowball scenario remains: a runaway eviction
   retry loop (e.g., from coord-side bookkeeping flakiness)
   can fill a 99 GB disk in ~12 minutes at 4 GiB per attempt.
   Add a host-side disk-pressure detector that pauses idle-
   eviction when free disk < N × `memory_mib` × 2 (worst-case
   peak during a 4 GiB FC dump). Surface as a Prometheus
   gauge so dashboards alert before fill. Lower urgency than
   #1–#3 but the right backstop — defense in depth.

5. **Warm-pool refill failures don't surface to coord.**
   The 12-minute window when `xngk` was filling disk had no
   coord-side WARN logs at all. Coord only started logging
   eviction failure once disk was full and `create_dir_all`
   ENOSPC'd. The refill-failure path is host-internal; coord
   doesn't get a heartbeat-level signal for "I'm failing to
   refill template X." Add a heartbeat field for per-template
   refill failure count/rate so coord can: (a) page on
   chronic refill failures, (b) drain hosts that can't
   refill anything (or restrict them to existing-session-only
   mode). Naturally pairs with #3's coord-side sweeper.

6. **(Closed by `3b6aec3`) host-agent logs missing from Cloud
   Logging.** Cloud Logging ingested coord/web pod logs but
   not the host-agent's systemd journal. Investigations
   required `sudo journalctl` per FC host VM, gated on
   org-level IAM and only useful in real-time. Closed by the
   Ops Agent install in
   `deploy/packer/provisioners/gcp/install-ops-agent.sh`.
   Journal now queryable via:
   ```
   resource.type="gce_instance" AND
   jsonPayload._SYSTEMD_UNIT="engram-host-agent.service"
   ```
   Documented as a closed item so a future reader knows the
   diagnostic capability was added in response to this
   investigation, not as a planned feature. Follow-up:
   update `~/.claude/skills/engrams-prod-ops/scripts/logs-host.sh`
   to prefer Cloud Logging over SSH+`journalctl`, killing
   the sudo dependency for host-side log access.

## Related

- ADR 0007: chunked immutable storage — provides the chunked-memory
  substrate this ADR consumes.
- ADR 0009: state reconciliation — extends the dead-host reaper here.
- ADR 0011: HostClient trait — extends the trait surface here.
- ADR 0012: pull-based host dispatch — superseded by this ADR.
- ADR 0013: stateless transport — enables the per-session-create
  parallel-ask scheduling pattern this ADR uses.
