//! ADR 0047: PG-authoritative placement.
//!
//! Every scheduling decision reads the `hosts` rows (heartbeat-persisted
//! capacity + readiness + snapshot locality + the coordinator-owned
//! `cordoned` bit) so any coordinator replica picks from the same
//! authority. The old in-memory `HostState` mirror is gone — the
//! [`crate::host_registry::HostRegistry`] only resolves a picked
//! `HostId` to a dialable backend.
//!
//! Shape: pure ranking/pick functions over `&[HostRecord]` (fully
//! unit-tested, no I/O) + thin async wrappers that read
//! `list_active_hosts()` and resolve the backend. The create path keeps
//! the ADR 0046 split — [`candidates_for`] feeds
//! `MetadataStore::reserve_placement`, whose `FOR UPDATE` transaction
//! remains the only place capacity is *committed*; the resume/evac path
//! ([`pick_for_session`]) stays capacity-soft (the pre-existing ADR 0046
//! gap, narrowed by ADR 0048's preview, not widened here).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use engram_core::traits::{HostClient, MetadataStore, SessionFence};
use engram_core::types::host::{CapStatus, HostRecord, HostStatus, ReservedBudget};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{HostId, SandboxError, SandboxId};
use engram_protocol::heartbeat::ManifestDigest;

use crate::host_registry::HostRegistry;

/// Inputs the scheduler considers when picking a host. (Moved verbatim
/// from `host_registry` by ADR 0047 — the semantics are unchanged, only
/// the backing store moved from the in-memory mirror to the hosts rows.)
#[derive(Clone, Debug)]
pub struct ScheduleContext<'a> {
    pub repo: &'a str,
    pub image_version: &'a str,
    /// ADR 0078 (GCS-free resume) tier-0 authoritative affinity: the host
    /// that holds this snapshot's chunks on its NVMe, read from the ONE
    /// owner of that fact — `SnapshotRecord::host_id` in PG. `None` for
    /// fresh sessions, or for a resume whose snapshot row has a NULL
    /// `host_id` (the capturing host was already deleted). Replaces the
    /// dead `prefer_snapshot_id × HostRecord::local_snapshots` tier (the
    /// heartbeat mirror was never populated). When `Some` and the host is
    /// schedulable with disk + RAM/CPU headroom, `pick_from` selects it
    /// authoritatively; every veto is counted, never silent.
    pub snapshot_host: Option<HostId>,
    /// Memory hint for capacity ranking. `None` falls through to
    /// "any host with > 0 free capacity" rather than a strict fit.
    pub memory_mib: Option<u32>,
    /// ADR 0048: vCPU budget for CPU-dimension ranking on the resume/evac
    /// path. `None`/0 means "no CPU gate" (the create path commits CPU in
    /// `reserve_placement`, not here).
    pub cpu_budget_vcpus: Option<u32>,
    /// ADR 0015 M5: when set, restricts the candidate pool to hosts
    /// whose latest heartbeat reported this digest in `ready_images`.
    pub required_image_digest: Option<ManifestDigest>,
    /// ADR 0018 Phase C: the picker excludes this host (evacuation
    /// must never re-target the source).
    pub exclude_host: Option<HostId>,
    /// ADR 0039 / ADR 0045 D4: SOFT host preference — the session's
    /// last host (warm chunk cache + base shm). Falls through on any
    /// miss; ranked below snapshot-affinity.
    pub prefer_host: Option<HostId>,
    /// ADR 0068: capability requirements this placement actually
    /// needs. `Default` (both fields `false`/`None`) imposes no
    /// capability constraint beyond the base gate every FC placement
    /// gets (`grpc_self_connect` + `bundle_stamp` — see
    /// `host_meets_capabilities`).
    pub caps: CapabilityRequirements,
    /// ADR 0090: SOFT ordering preference — the session's pinned aux
    /// bundle generations (harness/skills squashfs). `rank_hosts` puts
    /// hosts whose heartbeat-reported `current_bundles` stamp covers
    /// every sha here FIRST, steering resume/recovery placements away
    /// from freshly-provisioned nodes whose bundle staging hasn't
    /// finished (campaign B1: a relocation landed on a 25-min-old node
    /// and the harness spawn found no `/opt/engram/dyn/0/harness`).
    /// Soft — an uncovered host still places (the restore-side
    /// `materialize_if_missing` + the start_agent retry budget own
    /// correctness); empty imposes no ordering.
    pub prefer_bundles: &'a [engram_core::types::sandbox::AuxBundleRef],
}

/// ADR 0068: what a specific placement needs from a host's capability
/// vector, derived by the caller from the session/image/snapshot being
/// placed — NOT a static property of the host. `host_meets_capabilities`
/// is the pure predicate that reads a [`HostRecord`] against this.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityRequirements {
    /// This placement restores/creates against a memory manifest, so
    /// it needs the FC UFFD substrate: `base_shm_tmpfs` +
    /// `uffd_minor_shmem` + `nbd` must all be `Ok` on the candidate
    /// host. Derived per call site — e.g. `snapshot.memory_manifest.is_some()`
    /// on resume, the enabled image's `base_snapshot_memory_manifest`
    /// presence on create.
    pub needs_uffd_substrate: bool,
    /// The snapshot row's recorded capture-time FC snapshot version,
    /// when known (`SnapshotRecord::fc_snapshot_version`). `Some(v)`
    /// requires the candidate host's reported `fc_snapshot_version` to
    /// equal `v` exactly — closes the cross-`SNAPSHOT_VERSION` restore-
    /// corruption class at placement instead of at guest-boot failure.
    /// `None` (pre-migration snapshot rows, VZ/Process) imposes no
    /// constraint.
    pub fc_snapshot_version: Option<String>,
}

/// ADR 0068: does `h`'s capability vector satisfy `req`? Pure — no I/O,
/// unit-tested directly against a matrix of vectors.
///
/// Gate semantics:
/// - `h.capabilities.schema == 0` (never reported — a pre-0068 row, or
///   a host mid-roll between the coord and host-agent deploys) passes
///   everything: the same soft posture `host_wire_version_ok` gives
///   `wire_version == 0`. Once the fleet rolls, every row carries a
///   real vector.
/// - `schema >= 1`: `grpc_self_connect` and `bundle_stamp` must be `Ok`
///   for ANY FC placement (a host that can't prove its own gRPC
///   listener is up, or has no bundle generation staged, can't safely
///   take any session). Each capability `req` actually asks for
///   (`needs_uffd_substrate` → `base_shm_tmpfs` + `uffd_minor_shmem` +
///   `nbd`) must be `Ok` **or** `NotApplicable` — `NotApplicable` is
///   the honest report of an ADR 0022 File-backend host (the substrate
///   was never configured, so there's nothing to probe): it must not
///   be treated the same as `Failed`/`Unknown`, or a File-mode fleet
///   is 100% `NoCapacity` for every memory-manifest placement. Only
///   `Failed` (probe ran, broke) and `Unknown` (never probed, but
///   `schema >= 1` so it should have been) fail a *required*
///   capability.
/// - `fc_snapshot_version`: when `req` names a version AND the host
///   reports one, they must match exactly. Either side being `None`
///   imposes no constraint.
pub fn host_meets_capabilities(
    h: &HostRecord,
    req: &CapabilityRequirements,
) -> Result<(), &'static str> {
    let caps = &h.capabilities;
    if caps.schema == 0 {
        return Ok(());
    }
    if !caps.grpc_self_connect.is_ok() {
        return Err("grpc_self_connect");
    }
    if !caps.bundle_stamp.is_ok() {
        return Err("bundle_stamp");
    }
    // A required substrate capability passes when the probe ran and
    // succeeded (`Ok`) OR when the host honestly reports it doesn't
    // apply (`NotApplicable` — e.g. an ADR 0022 File-backend host that
    // never configured the UFFD substrate). Only `Failed`/`Unknown`
    // withhold placement.
    let substrate_ok = |c: &CapStatus| matches!(c, CapStatus::Ok(_) | CapStatus::NotApplicable);
    if req.needs_uffd_substrate {
        if !substrate_ok(&caps.base_shm_tmpfs) {
            return Err("base_shm_tmpfs");
        }
        if !substrate_ok(&caps.uffd_minor_shmem) {
            return Err("uffd_minor_shmem");
        }
        if !substrate_ok(&caps.nbd) {
            return Err("nbd");
        }
    }
    if let (Some(want), Some(have)) = (&req.fc_snapshot_version, &caps.fc_snapshot_version) {
        if want != have {
            // The exclusion reason is a static str; surface the actual pair
            // here — a mismatch between two hosts running the same binary
            // is otherwise undiagnosable from the NoCapacity summary alone.
            tracing::warn!(
                host_id = %h.id,
                want = %want,
                have = %have,
                "fc_snapshot_version gate mismatch"
            );
            return Err("fc_snapshot_version");
        }
    }
    Ok(())
}

/// ADR 0084 §C: how much host disk a capture/materialize job needs —
/// threaded into [`capture_candidate_hosts`] so placement can veto a host that
/// technically clears the ADR 0078 disk FLOOR but doesn't have headroom
/// for the job's own write volume (the old picker only checked the
/// floor, never the job's size).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureFootprint {
    pub disk_mib: u64,
}

impl CaptureFootprint {
    /// A base-snapshot capture writes: the materialized rootfs (already
    /// on disk from the `materializing` stage, ≈1×image) PLUS the
    /// capture VM's own COW/scratch overlay of that rootfs (another
    /// ≈1×image) PLUS the memory dump (`mem_mib`, chunked+uploaded
    /// before the scratch is freed) PLUS a fixed slack for the host's
    /// other bookkeeping (state.bin/sidecar, temp chunk-store staging).
    pub fn for_capture(image_size_mib: u64, mem_mib: u64) -> Self {
        Self {
            disk_mib: 2 * image_size_mib + mem_mib + 4096,
        }
    }

    /// A materialize pulls the OCI layers, flattens them, and packs a
    /// bootable ext4 — roughly 2.5x the final image size once you
    /// count the flattened-layer scratch tree alongside the packed
    /// ext4 output, plus a fixed slack for the OCI pull cache.
    pub fn for_materialize(image_size_mib: u64) -> Self {
        Self {
            disk_mib: (image_size_mib as f64 * 2.5).ceil() as u64 + 1024,
        }
    }

    /// Floor-only sizing: imposes NO footprint veto beyond the ADR 0078
    /// disk floor `host_disk_floor_ok` already checks. Used ONLY when
    /// the job's image size is genuinely unknown at pick time — today
    /// that's every `materialize_image_on_host` call, because
    /// materialize is what PRODUCES the disk manifest a size could be
    /// read from; there is no size signal to read yet (the OCI
    /// manifest's compressed layer sizes are a poor proxy for the
    /// flattened+packed ext4 output, so we deliberately don't guess
    /// from them). LOUD by design: every call site that reaches for
    /// this must justify why in a comment, not use it as a silent
    /// default.
    pub const fn floor_only() -> Self {
        Self { disk_mib: 0 }
    }
}

/// Why a pick couldn't place a session.
#[derive(Clone, Debug)]
pub enum PickError {
    /// No schedulable host has prefetched this digest.
    ImageNotReady(ManifestDigest),
    /// At least one host is schedulable (or no digest was required) but
    /// none can take the session.
    NoCapacity,
    /// The picked host couldn't be dialed (no addr / dial failure) —
    /// transient; the caller retries or the scanner re-picks.
    HostUnreachable(HostId, String),
    /// The hosts read itself failed (PG hiccup) — transient.
    Internal(String),
}

impl From<PickError> for SandboxError {
    fn from(e: PickError) -> Self {
        match e {
            PickError::ImageNotReady(d) => SandboxError::ImageNotReady(d.as_str().to_string()),
            PickError::NoCapacity => {
                SandboxError::Vm("no host has free capacity for this session".into())
            }
            // Both variants are transient by contract (see the enum docs):
            // map to the typed `Unavailable`, which the API layer surfaces
            // as a retryable 503 and the resume verb as `OpOutcome::Retry`.
            // The old catch-all `Vm` mapping landed `ApiError::Internal` →
            // terminal `Failed` — pre-0079 the wire caller retried around
            // that, but the verb now owns the only attempt.
            PickError::HostUnreachable(h, msg) => {
                SandboxError::Unavailable(format!("picked host {h} is unreachable: {msg}"))
            }
            PickError::Internal(msg) => SandboxError::Unavailable(format!("placement read: {msg}")),
        }
    }
}

/// ADR 0046/0047: ranked candidates for `reserve_placement` — the
/// snapshot-affinity hosts form the prefix (`hosts[..affinity_len]`),
/// the rest follow in row order.
#[derive(Clone, Debug, Default)]
pub struct RankedCandidates {
    pub hosts: Vec<HostId>,
    pub affinity_len: usize,
}

/// Fleet-wide autoscaling counts (ADR 0044 K4), derived from the hosts
/// rows so every replica reports the same numbers.
#[derive(Clone, Copy, Debug, Default)]
pub struct FleetSnapshot {
    /// Hosts with a fresh heartbeat.
    pub ready_hosts: u32,
    /// Fresh + `status=ready` + not cordoned — what the scheduler can
    /// place on.
    pub schedulable_hosts: u32,
    /// Σ `allocatable_mib` over schedulable hosts. ADR 0047 note: this
    /// was `Σ capacity.total_mib` pre-0047; allocatable is the honest
    /// "what one node adds" figure the autoscaler's per-host average
    /// wants (it nets out the daemon/OS/residency baseline).
    pub total_mib: u64,
    /// ADR 0048: Σ host CPU budget (`total_vcpus × overcommit`) over
    /// schedulable hosts — the per-host-average CPU capacity a node adds.
    pub total_vcpus: u64,
    /// ADR 0048: Σ max(0, cpu_budget − reserved_vcpus) over schedulable
    /// hosts — the spare vCPU the autoscaler scales the CPU dimension on.
    pub free_vcpus: u64,
    /// Σ max(0, allocatable_mib − reserved_mib) over schedulable hosts —
    /// the RAM demand-pressure signal (`/admin/fleet/demand` + the
    /// `engram_fleet_free_mib` gauge). Subsumes the retired
    /// `MetadataStore::fleet_free_mib`, whose SQL counted every
    /// ready/uncordoned host: a capability-failed or heartbeat-stale
    /// host that placement excludes was still counted as free capacity,
    /// so the autoscaler under-scaled while placement starved
    /// (2026-07-11 campaign: 3 "ready" hosts took zero sessions while
    /// creates queue-timed out).
    pub free_mib: u64,
    /// ADR 0047/0048: hosts cordoned off for scale-down — operator
    /// visibility into how much of the fleet is mid-drain.
    pub cordoned_hosts: u32,
}

/// Heartbeat-freshness horizon for placement (and the routing cache's
/// read-through). `ENGRAM_HOST_REGISTRY_TTL_SECS`, default 60s — well
/// over the 5s heartbeat cadence, under the dead-host detector's reach.
pub fn placement_ttl() -> Duration {
    let secs = std::env::var("ENGRAM_HOST_REGISTRY_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(60);
    Duration::from_secs(secs)
}

/// Issue #229: is `h`'s reported bincode wire version compatible with the
/// coordinator's? A host is excluded only when it reports a NONZERO
/// version that differs from ours — a real, observed skew mid rolling
/// deploy. `0` (not yet reported: a freshly-registered host before its
/// first heartbeat, or a pre-0066 row) is tolerated, the same soft
/// posture an unmeasured allocatable gets — excluding it would strand
/// brand-new hosts before they could report.
pub fn host_wire_version_ok(h: &HostRecord) -> bool {
    h.wire_version == 0 || h.wire_version == engram_protocol::WIRE_VERSION
}

/// A host the scheduler may place on: host-reported `ready` (a
/// `draining` status is the agent's own shutdown flag), not
/// coordinator-cordoned, heartbeat-fresh within `ttl`, and on a
/// compatible wire version (issue #229).
pub fn host_is_schedulable(h: &HostRecord, now: DateTime<Utc>, ttl: Duration) -> bool {
    h.status == HostStatus::Ready
        && !h.cordoned
        && host_wire_version_ok(h)
        && now
            .signed_duration_since(h.last_heartbeat_at)
            .to_std()
            .map_or(
                // last_heartbeat_at in the future (clock skew between the
                // writer pod and us) is trivially fresh.
                true,
                |age| age <= ttl,
            )
}

/// ADR 0095: a host that may SERVE chunks to a fleet peer — alive
/// (Ready + heartbeat-fresh), wire-compatible, with a dialable addr.
/// Deliberately NOT [`host_is_schedulable`]: a coordinator-cordoned
/// host mid-drain is often the one host that HOLDS the bytes (the
/// evacuation source; the resume source during a roll) and serving
/// reads costs it nothing schedulability protects. Returns the addr on
/// success so call sites can't forget the addr-present check.
pub fn host_can_serve_chunks(h: &HostRecord, now: DateTime<Utc>, ttl: Duration) -> Option<&str> {
    let fresh = now
        .signed_duration_since(h.last_heartbeat_at)
        .to_std()
        .map_or(true, |age| age <= ttl);
    if h.status == HostStatus::Ready && fresh && host_wire_version_ok(h) {
        h.host_addr.as_deref()
    } else {
        None
    }
}

fn host_passes_filters(
    h: &HostRecord,
    ctx: &ScheduleContext<'_>,
    now: DateTime<Utc>,
    ttl: Duration,
) -> bool {
    if Some(h.id) == ctx.exclude_host {
        return false;
    }
    if !host_is_schedulable(h, now, ttl) {
        return false;
    }
    if host_meets_capabilities(h, &ctx.caps).is_err() {
        return false;
    }
    match ctx.required_image_digest.as_ref() {
        Some(d) => h.ready_images.iter().any(|r| r == d.as_str()),
        None => true,
    }
}

/// ADR 0068: the first reason each host in `hosts` is excluded from
/// `ctx`, in row order — the "no capacity with free hosts" mystery mode
/// killer. `host_wire_version_ok` used to silently drop a skewed host
/// from the candidate set with the caller seeing only bare
/// `PickError::NoCapacity`; this makes the drop visible. Pure (no I/O) —
/// callers log/metric it themselves, only on the `NoCapacity` path (this
/// walks every host, so it's not free — don't call it on the happy
/// path).
///
/// One reason per host, first-match order: `excluded` (an explicit
/// `exclude_host`) → `not_ready` (`HostStatus != Ready`) → `cordoned` →
/// `wire_skew` → `stale` (heartbeat older than `ttl`) → `cap:<name>` (a
/// required capability failed) → `digest_not_ready` (image not
/// prefetched) → `no_fit` (schedulable but this predicate found nothing
/// else wrong — a capacity-dimension miss the caller's own fit logic
/// will re-discover).
pub fn exclusion_summary(
    hosts: &[HostRecord],
    ctx: &ScheduleContext<'_>,
    now: DateTime<Utc>,
    ttl: Duration,
) -> Vec<(HostId, String)> {
    hosts
        .iter()
        .map(|h| {
            let reason = if Some(h.id) == ctx.exclude_host {
                "excluded".to_string()
            } else if h.status != HostStatus::Ready {
                "not_ready".to_string()
            } else if h.cordoned {
                "cordoned".to_string()
            } else if !host_wire_version_ok(h) {
                "wire_skew".to_string()
            } else if now
                .signed_duration_since(h.last_heartbeat_at)
                .to_std()
                .is_ok_and(|age| age > ttl)
            {
                "stale".to_string()
            } else if let Err(cap) = host_meets_capabilities(h, &ctx.caps) {
                format!("cap:{cap}")
            } else if ctx
                .required_image_digest
                .as_ref()
                .is_some_and(|d| !h.ready_images.iter().any(|r| r == d.as_str()))
            {
                "digest_not_ready".to_string()
            } else {
                "no_fit".to_string()
            };
            (h.id, reason)
        })
        .collect()
}

/// Pure ranking: filter to schedulable candidates and put the
/// snapshot-affinity hosts first. Row order (PK-sorted from
/// `list_active_hosts`) breaks ties deterministically.
pub fn rank_hosts(
    hosts: &[HostRecord],
    ctx: &ScheduleContext<'_>,
    now: DateTime<Utc>,
    ttl: Duration,
) -> RankedCandidates {
    // ADR 0078: the authoritative `snapshot_host` tier-0 is decided in
    // `pick_from` (it needs the reserved-budget map for the RAM/CPU +
    // disk-headroom veto). `rank_hosts` just yields the schedulable
    // candidate set in row order; there is no longer a capacity-blind
    // affinity prefix baked in here (the dead `local_snapshots` tier).
    let mut ranked: Vec<HostId> = Vec::new();
    // ADR 0090: bundle-covered hosts first (soft, stable — row order
    // preserved within each tier). Uncovered hosts still rank; they just
    // lose ties, so a recovery avoids a mid-staging node when any
    // alternative exists but is never stranded when none does.
    let mut uncovered: Vec<HostId> = Vec::new();
    for h in hosts {
        if host_passes_filters(h, ctx, now, ttl) {
            if host_covers_bundles(h, ctx.prefer_bundles) {
                ranked.push(h.id);
            } else {
                uncovered.push(h.id);
            }
        }
    }
    ranked.extend(uncovered);
    RankedCandidates {
        hosts: ranked,
        affinity_len: 0,
    }
}

/// Does `h`'s heartbeat-reported bundle stamp cover every preferred sha?
/// Sha-only containment — the stamp and the snapshot pin reference the
/// same content-addressed generations, but drive-id spelling is a
/// slot-assignment detail. Empty prefer set ⇒ trivially covered.
pub fn host_covers_bundles(
    h: &HostRecord,
    prefer: &[engram_core::types::sandbox::AuxBundleRef],
) -> bool {
    prefer
        .iter()
        .all(|p| h.current_bundles.iter().any(|b| b.sha256 == p.sha256))
}

/// Pure pick for the resume/evac path. Ranking tiers:
///
/// 1. snapshot-affinity (capacity-CHECKED: the tier-0 veto mirrors every
///    `host_passes_filters` gate plus disk/RAM/CPU — an affinity host that
///    can't take the resume falls through to the soft tiers, counted in
///    `engram_resume_affinity_fallback_total`),
/// 2. `prefer_host` if it passes the same disk + RAM/CPU fit gate as
///    tier 0 (`named_host_fit_veto` — shared so a host tier-0 vetoed,
///    e.g. for `disk_full`, can't be re-picked via prefer),
/// 3. BEST-FIT: smallest free RAM among hosts that fit BOTH `memory_mib`
///    and the CPU budget (ADR 0048 — pack, don't spread),
/// 4. fallback: the first ranked candidate (hosts without an
///    allocatable measurement yet, or nothing fits — this path is
///    deliberately capacity-soft; only `reserve_placement` commits).
///
/// "free RAM" is `allocatable_mib − reserved.mem_mib` (ADR 0046's real
/// headroom); "free vCPU" is `total_vcpus × overcommit − reserved.vcpus`
/// (ADR 0048; a host that hasn't reported its core count has budget 0 →
/// no CPU gate, the soft posture).
pub fn pick_from(
    hosts: &[HostRecord],
    reserved: &HashMap<HostId, ReservedBudget>,
    ctx: &ScheduleContext<'_>,
    now: DateTime<Utc>,
    ttl: Duration,
) -> Result<HostId, PickError> {
    pick_from_2d(hosts, reserved, ctx, now, ttl, false)
}

/// The ranked 2D pick, with an explicit `require_fit` knob.
///
/// `require_fit=false` (the historical `pick_from` behavior) keeps the
/// ADR 0046 capacity-SOFT last-resort fallback: when nothing fits both
/// budgets, place on the first-ranked host anyway (a resume/evac never
/// stranded on a blip). `require_fit=true` is the #800 RESERVED-placement
/// mode: it drops that fallback and returns `NoCapacity` when no ranked host
/// fits — the HARD reserved bound the create path already honors. The evac
/// resumer uses it so a drain-driven relocation queues (honest overflow)
/// rather than binding a measured-full survivor and driving Σ reserved >
/// allocatable (the #722/#795 over-reservation class on the evac leg).
///
/// The named steering tiers (tier-0 snapshot affinity, tier-2 prefer) are
/// unaffected: they already carry their own `ram_full`/`cpu_full` vetoes
/// (`named_host_fit_veto`), so a full named host is vetoed and falls through
/// to best-fit under BOTH modes; only the terminal fallback differs.
pub fn pick_from_2d(
    hosts: &[HostRecord],
    reserved: &HashMap<HostId, ReservedBudget>,
    ctx: &ScheduleContext<'_>,
    now: DateTime<Utc>,
    ttl: Duration,
    require_fit: bool,
) -> Result<HostId, PickError> {
    let ranked = rank_hosts(hosts, ctx, now, ttl);
    if ranked.hosts.is_empty() {
        return match ctx.required_image_digest.as_ref() {
            Some(d) => Err(PickError::ImageNotReady(d.clone())),
            None => Err(PickError::NoCapacity),
        };
    }
    let need_mib = ctx.memory_mib.unwrap_or(0) as i64;
    let need_vcpus = ctx.cpu_budget_vcpus.unwrap_or(0) as i64;
    // Free RAM for `id`: None ⇒ unmeasured (treated as "fits" softly).
    let free_mib_of = |id: HostId| -> Option<i64> {
        let h = hosts.iter().find(|h| h.id == id)?;
        let alloc = h.utilization.allocatable_mib as i64;
        if alloc <= 0 {
            return None;
        }
        Some(alloc - reserved.get(&id).map(|r| r.mem_mib).unwrap_or(0))
    };
    // CPU fit for `id`: a host with budget 0 (unreported) doesn't gate.
    let cpu_fits = |id: HostId| -> bool {
        let Some(h) = hosts.iter().find(|h| h.id == id) else {
            return false;
        };
        let budget = engram_core::types::host::host_cpu_budget(h.total_vcpus);
        if budget <= 0 {
            return true;
        }
        budget - reserved.get(&id).map(|r| r.vcpus).unwrap_or(0) >= need_vcpus
    };
    // Tier 0 (ADR 0078): authoritative snapshot-host affinity. If the
    // host that holds this snapshot's chunks is alive, schedulable, and
    // has disk + RAM/CPU headroom, place there ALWAYS — its NVMe already
    // holds the divergent set, so the resume is a local read. Every veto
    // is counted (`engram_resume_affinity_fallback_total{reason}`) so the
    // fallback is never again silent; then we fall through to the soft
    // tiers below, exactly as a fresh session would.
    if let Some(sh) = ctx.snapshot_host {
        match snapshot_host_veto(sh, hosts, ctx, now, ttl, &free_mib_of, &cpu_fits) {
            None => return Ok(sh),
            Some(reason) => {
                metrics::counter!(
                    "engram_resume_affinity_fallback_total",
                    "reason" => reason.into_owned(),
                )
                .increment(1);
            }
        }
    }
    // 2. soft host-affinity. Membership in `ranked` covers the
    //    schedulability gates; `named_host_fit_veto` covers disk +
    //    RAM/CPU — the SAME gate tier-0 applies. Without it, a
    //    snapshot host tier-0 just vetoed (e.g. `disk_full`) is
    //    re-picked here, because the resume path passes the same host
    //    as `prefer_host` — nullifying the veto and falsifying the
    //    fallback metric.
    if let Some(want) = ctx.prefer_host {
        if ranked.hosts.contains(&want) {
            if let Some(h) = hosts.iter().find(|h| h.id == want) {
                if named_host_fit_veto(h, ctx, &free_mib_of, &cpu_fits).is_none() {
                    return Ok(want);
                }
            }
        }
    }
    // 3. best-fit: SMALLEST measured free RAM that fits both dims.
    let mut best: Option<(i64, HostId)> = None;
    for &id in &ranked.hosts {
        let Some(free) = free_mib_of(id) else {
            continue;
        };
        if free < need_mib || !cpu_fits(id) {
            continue;
        }
        match best {
            Some((bf, _)) if bf <= free => {}
            _ => best = Some((free, id)),
        }
    }
    if let Some((_, id)) = best {
        return Ok(id);
    }
    // 4. No MEASURED host fits both budgets.
    if require_fit {
        // RESERVED mode (#800): honor the hard bound — but an UNMEASURED host
        // (allocatable == 0: brand-new / dev / non-Linux) still counts as
        // fitting, exactly the last-resort soft posture `placement_preview`,
        // `reserve_placement`/`pick_host_2d`, and the sim's queue re-placement
        // all take. Consistency matters: if the reserved evac pick returned
        // NoCapacity here while the queue scanner's `placement_preview` said
        // "fits" on the same unmeasured fleet, a queued evac would churn
        // Queued↔Idle forever (the #795 livelock, reincarnated). So only a
        // fleet where every MEASURED host is full AND no unmeasured host
        // exists is a true no-fit that queues.
        if let Some(&id) = ranked
            .hosts
            .iter()
            .find(|&&id| free_mib_of(id).is_none() && cpu_fits(id))
        {
            return Ok(id);
        }
        return Err(PickError::NoCapacity);
    }
    // Otherwise: capacity-soft last-resort fallback (the pre-existing
    // ADR 0046 resume/evac posture — place on the first-ranked host).
    Ok(ranked.hosts[0])
}

/// ADR 0078 tier-0: why (if at all) the authoritative `snapshot_host` is
/// NOT usable for this placement — the label for
/// `engram_resume_affinity_fallback_total{reason}`. `None` means "usable,
/// place there". First-match order mirrors [`exclusion_summary`], then
/// adds the disk + capacity vetoes tier-0 layers on top:
/// `dead` (gone from the fleet / not Ready / stale heartbeat) → `cordoned`
/// → `wire_skew` → `cap:<name>` / `digest_not_ready` (schedulability) →
/// `disk_full` (free work_dir below the chunk-cache floor — a host about
/// to disk-evict its cache is a pointless affinity target) → `ram_full` /
/// `cpu_full` (won't fit the session's committed budget).
fn snapshot_host_veto(
    sh: HostId,
    hosts: &[HostRecord],
    ctx: &ScheduleContext<'_>,
    now: DateTime<Utc>,
    ttl: Duration,
    free_mib_of: &impl Fn(HostId) -> Option<i64>,
    cpu_fits: &impl Fn(HostId) -> bool,
) -> Option<std::borrow::Cow<'static, str>> {
    let Some(h) = hosts.iter().find(|h| h.id == sh) else {
        // The capturing host row is gone (deleted / never re-registered).
        return Some("dead".into());
    };
    // Schedulability, in the same order `exclusion_summary` reports.
    if Some(h.id) == ctx.exclude_host {
        return Some("excluded".into());
    }
    if h.status != HostStatus::Ready {
        return Some("dead".into());
    }
    if h.cordoned {
        return Some("cordoned".into());
    }
    if !host_wire_version_ok(h) {
        return Some("wire_skew".into());
    }
    if now
        .signed_duration_since(h.last_heartbeat_at)
        .to_std()
        .is_ok_and(|age| age > ttl)
    {
        return Some("dead".into());
    }
    if let Err(cap) = host_meets_capabilities(h, &ctx.caps) {
        // `cap:<name>` — a fleet-wide fallback spike labeled by WHICH
        // capability failed (fc_snapshot_version vs bundle_stamp vs
        // base_shm_tmpfs need very different remediations).
        return Some(format!("cap:{cap}").into());
    }
    if ctx
        .required_image_digest
        .as_ref()
        .is_some_and(|d| !h.ready_images.iter().any(|r| r == d.as_str()))
    {
        return Some("digest_not_ready".into());
    }
    // Disk + capacity: the shared named-host fit gate (also applied by
    // the tier-2 prefer arm) so the two can never drift.
    named_host_fit_veto(h, ctx, free_mib_of, cpu_fits).map(Into::into)
}

/// The disk + capacity gates any NAMED steering target must pass — the
/// tier-0 `snapshot_host` and the tier-2 `prefer_host` alike. These are
/// exactly the vetoes `rank_hosts` does NOT enforce (a ranked host can
/// still be disk-pressured or budget-full), so every arm that picks a
/// specific host by name must apply them; one shared predicate instead of
/// two drifting copies. Returns the
/// `engram_resume_affinity_fallback_total{reason}` label, `None` = fits.
///
/// - `disk_full`: free work_dir below the chunk-cache floor — the host is
///   about to disk-evict its cache, a pointless locality target.
///   `disk_total_mib == 0` (unmeasured) is soft (no veto), same posture
///   as unmeasured RAM.
/// - `ram_full` / `cpu_full`: the session's committed budget doesn't fit
///   (unmeasured RAM / an unreported CPU budget are soft).
fn named_host_fit_veto(
    h: &HostRecord,
    ctx: &ScheduleContext<'_>,
    free_mib_of: &impl Fn(HostId) -> Option<i64>,
    cpu_fits: &impl Fn(HostId) -> bool,
) -> Option<&'static str> {
    if !host_disk_floor_ok(h) {
        return Some("disk_full");
    }
    let need_mib = ctx.memory_mib.unwrap_or(0) as i64;
    if free_mib_of(h.id).is_some_and(|free| free < need_mib) {
        return Some("ram_full");
    }
    if !cpu_fits(h.id) {
        return Some("cpu_full");
    }
    None
}

async fn hosts_and_reserved(
    meta: &dyn MetadataStore,
) -> Result<(Vec<HostRecord>, HashMap<HostId, ReservedBudget>), PickError> {
    let hosts = meta
        .list_active_hosts()
        .await
        .map_err(|e| PickError::Internal(format!("list_active_hosts: {e}")))?;
    let reserved = meta
        .per_host_reserved()
        .await
        .map_err(|e| PickError::Internal(format!("per_host_reserved: {e}")))?;
    Ok((hosts, reserved))
}

/// ADR 0048 C8 (drain don't-strand guard): is there a schedulable host
/// (matching `ctx`, e.g. with `exclude_host` set to the drain victim)
/// that fits BOTH budgets? A HARD 2D check — unlike `pick_from`'s
/// capacity-soft fallback — so a drain doesn't start a move that would
/// strand the session when the fleet is genuinely full. An unmeasured
/// host (no allocatable / no reported vCPU) counts as fitting (same soft
/// posture `reserve_placement` takes for brand-new / dev hosts).
pub async fn placement_preview(
    meta: &dyn MetadataStore,
    ctx: &ScheduleContext<'_>,
    mem_mib: i64,
    cpu_vcpus: i64,
    now: DateTime<Utc>,
) -> Result<bool, PickError> {
    let (hosts, reserved) = hosts_and_reserved(meta).await?;
    let ranked = rank_hosts(&hosts, ctx, now, placement_ttl());
    Ok(ranked.hosts.iter().any(|id| {
        let Some(h) = hosts.iter().find(|h| h.id == *id) else {
            return false;
        };
        let alloc = h.utilization.allocatable_mib as i64;
        if alloc <= 0 {
            return true; // unmeasured → soft fallback fits
        }
        let free_mib = alloc - reserved.get(id).map(|r| r.mem_mib).unwrap_or(0);
        if free_mib < mem_mib {
            return false;
        }
        let cpu_budget = engram_core::types::host::host_cpu_budget(h.total_vcpus);
        if cpu_budget > 0 {
            let free_vcpus = cpu_budget - reserved.get(id).map(|r| r.vcpus).unwrap_or(0);
            if free_vcpus < cpu_vcpus {
                return false;
            }
        }
        true
    }))
}

/// ADR 0046: the ranked candidates for the create path's
/// `reserve_placement` transaction.
pub async fn candidates_for(
    meta: &dyn MetadataStore,
    ctx: &ScheduleContext<'_>,
    now: DateTime<Utc>,
) -> Result<RankedCandidates, PickError> {
    let hosts = meta
        .list_active_hosts()
        .await
        .map_err(|e| PickError::Internal(format!("list_active_hosts: {e}")))?;
    Ok(rank_hosts(&hosts, ctx, now, placement_ttl()))
}

/// ADR 0068 (core-ops-batch correction pass): shared exclusion-visibility
/// helper for every "empty candidate set" placement failure — not just
/// `pick_for_session`'s `NoCapacity`. `#564`'s review found three other
/// call sites (`api/sessions.rs`'s create path, and `queue_scanner.rs`'s
/// `place_create` + `resume_has_capacity`) that returned an empty
/// candidate set with zero visibility into why, so a wire-skewed or
/// capability-failing fleet looked identical to a genuinely full one on
/// every path except resume. Call this whenever a candidate set turns up
/// empty; it re-reads `list_active_hosts` (this only runs on the
/// uncommon failure path, so the extra read doesn't cost the common
/// case), emits the bounded `exclusion_summary` per host as both the
/// `PLACEMENT_EXCLUDED_TOTAL` counter (labeled `origin` + `reason`) and a
/// `tracing::warn!` naming every host.
///
/// `origin` is one of the 4 values documented on
/// `metrics::PLACEMENT_EXCLUDED_TOTAL` — pass a `'static` string literal
/// from that fixed set so the label cardinality stays bounded.
pub async fn log_empty_candidates(
    meta: &dyn MetadataStore,
    ctx: &ScheduleContext<'_>,
    origin: &'static str,
    now: DateTime<Utc>,
) {
    let hosts = match meta.list_active_hosts().await {
        Ok(hosts) => hosts,
        Err(e) => {
            tracing::debug!(origin, error = %e,
                "log_empty_candidates: list_active_hosts failed; skipping exclusion visibility");
            return;
        }
    };
    let summary = exclusion_summary(&hosts, ctx, now, placement_ttl());
    for (_host_id, reason) in &summary {
        ::metrics::counter!(
            crate::metrics::PLACEMENT_EXCLUDED_TOTAL,
            "origin" => origin,
            "reason" => reason.clone(),
        )
        .increment(1);
    }
    tracing::warn!(
        repo = ctx.repo,
        image_version = ctx.image_version,
        origin,
        exclusions = ?summary,
        "placement: empty candidate set — per-host exclusion reasons",
    );
}

/// The sibling of [`log_empty_candidates`] for the OTHER silent-queue
/// branch: candidates existed (they passed schedulability/capability/
/// digest gates) but the FOR-UPDATE 2D pick fit none of them. Pre-fix,
/// this branch logged only a generic "no capacity — session queued"
/// with zero per-host figures (2026-07-11 campaign, ADR 0068's gap).
/// Emits one bounded reason per host — fit reasons (`ram_full` /
/// `cpu_full` / `unmeasured` / `not_lockable` / `fits_now`) for ranked
/// candidates via `MetadataStore::placement_no_fit_details`, and
/// `exclusion_summary` reasons for every active host that never made
/// the candidate set — as `PLACEMENT_EXCLUDED_TOTAL{origin,reason}`
/// plus one WARN line. Returns the combined per-host summary so the
/// caller can attach it to a durable event (`queue_timeout` payload).
pub async fn log_reserve_no_fit(
    meta: &dyn MetadataStore,
    ctx: &ScheduleContext<'_>,
    origin: &'static str,
    candidates: &[HostId],
    mem_budget_mib: i64,
    cpu_budget_vcpus: i32,
    now: DateTime<Utc>,
) -> Vec<(HostId, String)> {
    let mut summary: Vec<(HostId, String)> = Vec::new();
    match meta
        .placement_no_fit_details(candidates, mem_budget_mib, cpu_budget_vcpus)
        .await
    {
        Ok(details) => {
            for d in details {
                summary.push((
                    d.host_id,
                    format!(
                        "{} free_mib={} free_vcpus={}",
                        d.reason,
                        d.free_mib,
                        // i64::MAX = CPU-ungated host; render compactly.
                        if d.free_vcpus == i64::MAX {
                            -1
                        } else {
                            d.free_vcpus
                        }
                    ),
                ));
                ::metrics::counter!(
                    crate::metrics::PLACEMENT_EXCLUDED_TOTAL,
                    "origin" => origin,
                    "reason" => d.reason,
                )
                .increment(1);
            }
        }
        Err(e) => {
            tracing::debug!(origin, error = %e,
                "log_reserve_no_fit: placement_no_fit_details failed; fit reasons unavailable");
        }
    }
    // Hosts that never made the candidate set — the rank_hosts-side
    // exclusions, previously logged only when the whole set was empty.
    if let Ok(hosts) = meta.list_active_hosts().await {
        let non_candidates: Vec<_> = hosts
            .into_iter()
            .filter(|h| !candidates.contains(&h.id))
            .collect();
        for (host_id, reason) in exclusion_summary(&non_candidates, ctx, now, placement_ttl()) {
            ::metrics::counter!(
                crate::metrics::PLACEMENT_EXCLUDED_TOTAL,
                "origin" => origin,
                "reason" => reason.clone(),
            )
            .increment(1);
            summary.push((host_id, reason));
        }
    }
    tracing::warn!(
        repo = ctx.repo,
        image_version = ctx.image_version,
        origin,
        mem_budget_mib,
        cpu_budget_vcpus,
        exclusions = ?summary,
        "placement: candidates present but none fit — per-host reasons",
    );
    summary
}

/// Session scheduler for the resume/evac path: pick from the hosts rows
/// and resolve the backend (dialing through the PG `host_addr` when this
/// replica hasn't seen the host yet). Emits the ADR 0044 K4
/// demand-pressure counter on every decision.
pub async fn pick_for_session(
    meta: &dyn MetadataStore,
    registry: &HostRegistry,
    ctx: &ScheduleContext<'_>,
    now: DateTime<Utc>,
) -> Result<(HostId, Arc<dyn HostClient>), PickError> {
    let result = pick_for_session_inner(meta, registry, ctx, now, false).await;
    let outcome = match &result {
        Ok(_) => "placed",
        Err(PickError::NoCapacity) => "no_capacity",
        Err(PickError::ImageNotReady(_)) => "image_not_ready",
        Err(PickError::HostUnreachable(..)) => "host_unreachable",
        Err(PickError::Internal(_)) => "internal",
    };
    ::metrics::counter!(crate::metrics::SESSION_PLACEMENT_TOTAL, "outcome" => outcome).increment(1);
    // ADR 0068: on NoCapacity ONLY, name why — kills the "no capacity
    // with free hosts" mystery mode.
    if matches!(result, Err(PickError::NoCapacity)) {
        log_empty_candidates(meta, ctx, "resume", now).await;
    }
    result
}

/// #800: the RESERVED evac/resume placement — [`pick_for_session`] with the
/// capacity-soft fallback dropped. Returns `NoCapacity` when no schedulable
/// host fits the session's 2D budget (rather than binding the first-ranked,
/// possibly measured-full host), so the caller can QUEUE the session via the
/// reserved queue path (the #795 resume precedent) and re-home it once
/// capacity returns. The one commit point that mutates reservations stays
/// the caller's rebind (`assign_session_host`); this makes the *decision*
/// honor the hard bound. Emits the same K4 demand-pressure counter.
pub async fn pick_for_session_reserved(
    meta: &dyn MetadataStore,
    registry: &HostRegistry,
    ctx: &ScheduleContext<'_>,
    now: DateTime<Utc>,
) -> Result<(HostId, Arc<dyn HostClient>), PickError> {
    let result = pick_for_session_inner(meta, registry, ctx, now, true).await;
    let outcome = match &result {
        Ok(_) => "placed",
        Err(PickError::NoCapacity) => "no_capacity",
        Err(PickError::ImageNotReady(_)) => "image_not_ready",
        Err(PickError::HostUnreachable(..)) => "host_unreachable",
        Err(PickError::Internal(_)) => "internal",
    };
    ::metrics::counter!(crate::metrics::SESSION_PLACEMENT_TOTAL, "outcome" => outcome).increment(1);
    // On NoCapacity, name the per-host exclusion reasons (present-but-full
    // hosts show up via the ranked set; a fully-cordoned fleet via the
    // empty-candidate summary) — the reserved NoCapacity is the QUEUE
    // signal, not a mystery stall, but the visibility is still useful.
    if matches!(result, Err(PickError::NoCapacity)) {
        // `origin="resume"` — the bounded PLACEMENT_EXCLUDED_TOTAL vocabulary
        // already scopes the resume/evac path under this label (metrics.rs).
        log_empty_candidates(meta, ctx, "resume", now).await;
    }
    result
}

async fn pick_for_session_inner(
    meta: &dyn MetadataStore,
    registry: &HostRegistry,
    ctx: &ScheduleContext<'_>,
    now: DateTime<Utc>,
    require_fit: bool,
) -> Result<(HostId, Arc<dyn HostClient>), PickError> {
    let (hosts, reserved) = hosts_and_reserved(meta).await?;
    let id = pick_from_2d(&hosts, &reserved, ctx, now, placement_ttl(), require_fit)?;
    let backend = registry
        .backend_for(id)
        .await
        .map_err(|e| PickError::HostUnreachable(id, e.to_string()))?;
    Ok((id, backend))
}

/// ADR 0045 Phase F: validate + resolve a SPECIFIC host (operator-pinned
/// teleport target). Not image-readiness gated (the target
/// lazy-materializes); capacity-soft like the affinity tier, but a host
/// with a measured allocatable and no free RAM is rejected.
pub async fn pick_specific_host(
    meta: &dyn MetadataStore,
    registry: &HostRegistry,
    host_id: HostId,
    exclude_host: Option<HostId>,
    now: DateTime<Utc>,
) -> Result<(HostId, Arc<dyn HostClient>), PickError> {
    if Some(host_id) == exclude_host {
        return Err(PickError::NoCapacity);
    }
    let (hosts, reserved) = hosts_and_reserved(meta).await?;
    let ttl = placement_ttl();
    let Some(h) = hosts.iter().find(|h| h.id == host_id) else {
        return Err(PickError::NoCapacity);
    };
    if !host_is_schedulable(h, now, ttl) {
        return Err(PickError::NoCapacity);
    }
    // ADR 0068: an operator pin still gets the BASE gate (a host that
    // can't prove its own gRPC listener is up or has no bundle staged
    // can't safely take any session) — but not the requirement-specific
    // gates (`needs_uffd_substrate`/`fc_snapshot_version`), which stay
    // soft for an explicit pin.
    if host_meets_capabilities(h, &CapabilityRequirements::default()).is_err() {
        return Err(PickError::NoCapacity);
    }
    let alloc = h.utilization.allocatable_mib as i64;
    let reserved_mib = reserved.get(&host_id).map(|r| r.mem_mib).unwrap_or(0);
    if alloc > 0 && alloc - reserved_mib <= 0 {
        return Err(PickError::NoCapacity);
    }
    let backend = registry
        .backend_for(host_id)
        .await
        .map_err(|e| PickError::HostUnreachable(host_id, e.to_string()))?;
    Ok((host_id, backend))
}

/// ADR 0084 §C: the free work_dir disk (MiB) `capture_candidate_hosts` ranks
/// hosts by — `None` for an unmeasured host (dev/brand-new; the same
/// soft posture `host_disk_floor_ok` gives it: it neither vetoes NOR
/// ranks above a measured host, so a fleet of unmeasured hosts falls
/// back to row order, matching pre-ADR-0084 first-fit behavior).
fn free_disk_mib(h: &HostRecord) -> Option<u64> {
    if h.utilization.disk_total_mib == 0 {
        return None;
    }
    Some(
        h.utilization
            .disk_total_mib
            .saturating_sub(h.utilization.disk_used_mib),
    )
}

/// ADR 0084 §C: the pure filter+rank core of [`capture_candidate_hosts`] —
/// unit-tested directly against a `&[HostRecord]` fixture, no I/O.
///
/// Filters: schedulable ∧ the base capability gate (+ `fc_snapshot_version`
/// pin when `required_fc_version` names one) ∧ the ADR 0078 disk floor ∧
/// `need.disk_mib` headroom (an unmeasured host is soft here too — same
/// posture as the floor) ∧ NOT already running a live capture job
/// (`live_capture_hosts` — one-capture-per-host anti-affinity, ADR 0084
/// §C). Ranking: MAX free disk among survivors (replacing the old
/// first-fit) — row order breaks ties among unmeasured/equal hosts.
pub fn capture_candidate_hosts_from(
    hosts: &[HostRecord],
    live_capture_hosts: &std::collections::HashSet<HostId>,
    need: CaptureFootprint,
    required_fc_version: Option<&str>,
    now: DateTime<Utc>,
    ttl: Duration,
) -> Vec<HostId> {
    let caps = CapabilityRequirements {
        needs_uffd_substrate: false,
        fc_snapshot_version: required_fc_version.map(str::to_string),
    };
    let mut survivors: Vec<(u64, HostId)> = Vec::new();
    for h in hosts {
        if !host_is_schedulable(h, now, ttl) {
            continue;
        }
        if host_meets_capabilities(h, &caps).is_err() {
            continue;
        }
        if !host_disk_floor_ok(h) {
            continue;
        }
        if live_capture_hosts.contains(&h.id) {
            continue;
        }
        let free = free_disk_mib(h);
        if free.is_some_and(|free| free < need.disk_mib) {
            continue;
        }
        survivors.push((free.unwrap_or(0), h.id));
    }
    // Roomiest disk first — the candidate ORDER the atomic 2D RAM/CPU fit
    // (`pick_host_2d`) inherits, so a RAM-tie breaks toward the host with
    // the most disk headroom (replaces the P2 first-fit/max-free-disk
    // single-pick; the RAM/CPU dimension is now enforced in PG).
    survivors.sort_by_key(|(free, _)| std::cmp::Reverse(*free));
    survivors.into_iter().map(|(_, id)| id).collect()
}

/// ADR 0084 §C + (c): the CANDIDATE SET for a base-snapshot capture's
/// RESERVING pick — every host passing the ADR 0078 tier-0 disk floor,
/// the job's own [`CaptureFootprint`] disk headroom, one-capture-per-host
/// anti-affinity (`hosts_with_live_capture_jobs`), and an optional
/// `fc_snapshot_version` pin (ADR 0084 §B5), ordered roomiest-disk-first.
/// Deliberately RAM/CPU-BLIND here: the 2D fit + reservation happen
/// atomically in PG (`place_capture_job` / `reassign_capture_job` /
/// `redrive_failed_capture_job` → `pick_host_2d`), the same split
/// `capture_candidates`/`reserve_capture_host` had before this row moved
/// onto `capture_jobs`. An empty vec means no host is even disk-eligible
/// (the caller leaves the job WAITING, not failed).
pub async fn capture_candidate_hosts(
    meta: &dyn MetadataStore,
    need: CaptureFootprint,
    required_fc_version: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Vec<HostId>, PickError> {
    let hosts = meta
        .list_active_hosts()
        .await
        .map_err(|e| PickError::Internal(format!("list_active_hosts: {e}")))?;
    let live_capture_hosts = meta
        .hosts_with_live_capture_jobs()
        .await
        .map_err(|e| PickError::Internal(format!("hosts_with_live_capture_jobs: {e}")))?;
    Ok(capture_candidate_hosts_from(
        &hosts,
        &live_capture_hosts,
        need,
        required_fc_version,
        now,
        placement_ttl(),
    ))
}

/// ADR 0080 phase 3b: a host for the MATERIALIZE stage (docker pull +
/// ext4 pack + chunk) — no VM boots, so NOT gated on image readiness or
/// RAM/CPU capacity, but it IS gated on the ADR 0078 tier-0 disk floor:
/// materialize writes image-sized data under the host's work dir, so a
/// host already below the chunk-cache floor (about to disk-evict its
/// cache) must never be handed more disk work (the `capture-host picker
/// ignores disk` incident class). Distinct from [`capture_candidate_hosts`]:
/// materialize boots no VM, so it carries no [`CaptureFootprint`] /
/// anti-affinity / RAM reservation — those belong only to the capture
/// stage (ADR 0084 §C + the ADR 0081 reservation re-attach).
pub async fn pick_materialize_host(
    meta: &dyn MetadataStore,
    registry: &HostRegistry,
    now: DateTime<Utc>,
) -> Result<(HostId, Arc<dyn HostClient>), PickError> {
    let hosts = meta
        .list_active_hosts()
        .await
        .map_err(|e| PickError::Internal(format!("list_active_hosts: {e}")))?;
    let ttl = placement_ttl();
    let id = hosts
        .iter()
        .find(|h| capture_host_eligible(h, now, ttl))
        .map(|h| h.id)
        .ok_or(PickError::NoCapacity)?;
    let backend = registry
        .backend_for(id)
        .await
        .map_err(|e| PickError::HostUnreachable(id, e.to_string()))?;
    Ok((id, backend))
}

/// Shared eligibility for the enable pipeline's host-side stages:
/// schedulable, base-capable (ADR 0068 — see `pick_specific_host`), and
/// above the ADR 0078 tier-0 disk floor (same unmeasured-is-soft
/// posture as `named_host_fit_veto`).
fn capture_host_eligible(h: &HostRecord, now: DateTime<Utc>, ttl: Duration) -> bool {
    host_is_schedulable(h, now, ttl)
        && host_meets_capabilities(h, &CapabilityRequirements::default()).is_ok()
        && host_disk_floor_ok(h)
}

/// ADR 0078's tier-0 disk floor as a standalone predicate: free
/// work_dir space at or above the host disk-cache floor.
/// Unmeasured (`disk_total_mib == 0`) is soft — no veto, the same
/// posture as unmeasured RAM (brand-new / dev hosts). Kept in lockstep
/// with the disk arm of [`named_host_fit_veto`].
fn host_disk_floor_ok(h: &HostRecord) -> bool {
    if h.utilization.disk_total_mib == 0 {
        return true;
    }
    // ADR 0112 D5: subtract the host's COMMITTED swap (Σ swap_mib over
    // its live sandboxes) before the floor test — the backing files
    // are sparse, so `disk_used_mib` (statvfs) reflects only what
    // guests have swapped so far; the committed remainder is spoken
    // for and must not be handed to a new placement. Conservative by
    // design: already-allocated swap blocks appear in BOTH terms, so
    // this can only under-report free space, never over-report.
    let free_disk_mib = h
        .utilization
        .disk_total_mib
        .saturating_sub(h.utilization.disk_used_mib)
        .saturating_sub(h.utilization.committed_swap_mib);
    free_disk_mib >= host_disk_cache_floor_mib()
}

/// The placement disk floor gets its OWN env override — deliberately
/// NOT `ENGRAM_IDLE_EVICT_DISK_FLOOR_BYTES`, which the idle detector
/// (this process) and the host-agent's idle evictor already read.
/// Reusing that name would couple two independent knobs: tuning idle
/// eviction would silently also loosen placement's disk gate (tighter
/// packing → disk-full hosts). Small-disk dev rigs (the fc-colima VM)
/// set both.
fn host_disk_cache_floor_mib() -> u64 {
    std::env::var("ENGRAM_PLACEMENT_DISK_FLOOR_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(engram_core::types::host::HOST_DISK_CACHE_FLOOR_BYTES)
        / (1024 * 1024)
}

/// Pick a host for `ctx`, then `restore` from `metadata` on it. (The
/// pre-0047 `HostRegistry::restore_for_session`, relocated.) Caller is
/// responsible for `assign_session_host`.
///
/// `fence`: the resume verb's op epoch (ADR 0079) — stamps the restore
/// RPC so a superseded executor's restore is rejected host-side.
#[tracing::instrument(name = "coord.restore_for_session", skip_all)]
pub async fn restore_for_session(
    meta: &dyn MetadataStore,
    registry: &HostRegistry,
    ctx: &ScheduleContext<'_>,
    metadata: SnapshotMetadata,
    fence: SessionFence,
    now: DateTime<Utc>,
) -> Result<(HostId, SandboxId), SandboxError> {
    let (host_id, backend) = pick_for_session(meta, registry, ctx, now).await?;
    let sandbox_id = backend.restore(metadata, fence).await?;
    registry.record_sandbox_owner(sandbox_id, host_id);
    Ok((host_id, sandbox_id))
}

/// The capability posture the AUTOSCALER's counts assume: every prod
/// image is an FC memory-manifest placement, so a host whose UFFD
/// substrate probes fail can't take any of the demand the autoscaler is
/// sizing for. Counting such a host as capacity makes the scaler
/// complacent while placement starves (the 2026-07-11 "3 cold hosts"
/// mechanism). `NotApplicable` still passes (File-backend hosts,
/// ADR 0022); `fc_snapshot_version` imposes nothing here.
fn fleet_capability_req() -> CapabilityRequirements {
    CapabilityRequirements {
        needs_uffd_substrate: true,
        fc_snapshot_version: None,
    }
}

/// The autoscaler's per-host capacity predicate: a host counts only if
/// PLACEMENT would rank it — `host_is_schedulable` AND
/// `host_meets_capabilities` under the fleet posture. Pure so the
/// gate is unit-testable without a store.
pub fn host_counts_as_capacity(h: &HostRecord, now: DateTime<Utc>, ttl: Duration) -> bool {
    host_is_schedulable(h, now, ttl) && host_meets_capabilities(h, &fleet_capability_req()).is_ok()
}

/// Fleet-wide autoscaling counts (ADR 0044 K4 + ADR 0048 CPU dims),
/// from the hosts rows + the per-host reserved aggregate. A host counts
/// as schedulable capacity only per [`host_counts_as_capacity`] — so
/// the demand signal can never read healthier than the candidate set
/// placement actually ranks.
pub async fn fleet_snapshot(
    meta: &dyn MetadataStore,
    now: DateTime<Utc>,
) -> Result<FleetSnapshot, PickError> {
    let hosts = meta
        .list_active_hosts()
        .await
        .map_err(|e| PickError::Internal(format!("list_active_hosts: {e}")))?;
    let reserved = meta
        .per_host_reserved()
        .await
        .map_err(|e| PickError::Internal(format!("per_host_reserved: {e}")))?;
    let ttl = placement_ttl();
    let mut m = FleetSnapshot::default();
    for h in &hosts {
        let fresh = now
            .signed_duration_since(h.last_heartbeat_at)
            .to_std()
            .map_or(true, |age| age <= ttl);
        if !fresh {
            continue;
        }
        m.ready_hosts += 1;
        if h.cordoned {
            m.cordoned_hosts += 1;
        }
        if host_counts_as_capacity(h, now, ttl) {
            m.schedulable_hosts += 1;
            m.total_mib += h.utilization.allocatable_mib;
            let cpu_budget = engram_core::types::host::host_cpu_budget(h.total_vcpus);
            m.total_vcpus += cpu_budget as u64;
            let r = reserved.get(&h.id).copied().unwrap_or_default();
            m.free_vcpus += (cpu_budget - r.vcpus).max(0) as u64;
            m.free_mib += (h.utilization.allocatable_mib as i64 - r.mem_mib).max(0) as u64;
        }
    }
    Ok(m)
}

#[cfg(test)]
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use engram_core::types::host::{HostCapacity, HostMetadata, HostUtilization};

    fn host(id: u128) -> HostRecord {
        HostRecord {
            id: HostId(uuid::Uuid::from_u128(id)),
            hostname: format!("h{id}"),
            cloud_metadata: HostMetadata::default(),
            capacity: HostCapacity {
                total_gb: 0,
                used_gb: 0,
                total_mib: 0,
                used_mib: 0,
                running_sandboxes: 0,
            },
            utilization: HostUtilization::default(),
            status: HostStatus::Ready,
            last_heartbeat_at: Utc::now(),
            host_addr: None,
            ready_images: Vec::new(),
            current_bundles: Vec::new(),
            sandbox_bundles: Vec::new(),
            cordoned: false,
            total_vcpus: 0,
            // Issue #229: 0 = "not yet reported" → tolerated by the
            // placement filter. Tests that exercise the skew gate set this
            // to a concrete version explicitly.
            wire_version: 0,
            stages_images: false,
            capabilities: engram_core::types::host::HostCapabilities::default(),
            lease_expires_at: None,
            lease_state: Default::default(),
            lease_epoch: 0,
        }
    }

    fn hid(id: u128) -> HostId {
        HostId(uuid::Uuid::from_u128(id))
    }

    mod capability_gate {
        use super::*;
        use engram_core::types::host::CapStatus;

        fn caps_with(
            grpc: CapStatus,
            bundle: CapStatus,
            base_shm: CapStatus,
            uffd_minor: CapStatus,
            nbd: CapStatus,
            fc_snapshot_version: Option<&str>,
        ) -> engram_core::types::host::HostCapabilities {
            engram_core::types::host::HostCapabilities {
                schema: 1,
                backend: "firecracker".to_string(),
                grpc_self_connect: grpc,
                base_shm_tmpfs: base_shm,
                uffd_minor_shmem: uffd_minor,
                nbd,
                bundle_stamp: bundle,
                fc_snapshot_version: fc_snapshot_version.map(str::to_string),
                wire_version: 7,
            }
        }

        fn fully_ok() -> engram_core::types::host::HostCapabilities {
            caps_with(
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                None,
            )
        }

        /// `schema == 0` (never reported) passes EVERY requirement,
        /// including `needs_uffd_substrate` — the soft posture that
        /// keeps a pre-0068 row / mid-roll host placeable.
        #[test]
        fn schema_zero_passes_every_requirement() {
            let mut h = host(1);
            h.capabilities = engram_core::types::host::HostCapabilities::default();
            assert_eq!(h.capabilities.schema, 0);
            let req = CapabilityRequirements {
                needs_uffd_substrate: true,
                fc_snapshot_version: Some("v10.0.0".to_string()),
            };
            assert!(host_meets_capabilities(&h, &req).is_ok());
        }

        /// `schema >= 1` with no substrate/version requirement still
        /// demands the BASE gate: grpc_self_connect + bundle_stamp.
        #[test]
        fn base_gate_required_even_with_no_extra_requirement() {
            let mut h = host(1);
            h.capabilities = caps_with(
                CapStatus::Failed("connect refused".into()),
                CapStatus::Ok(None),
                CapStatus::NotApplicable,
                CapStatus::NotApplicable,
                CapStatus::NotApplicable,
                None,
            );
            assert_eq!(
                host_meets_capabilities(&h, &CapabilityRequirements::default()),
                Err("grpc_self_connect"),
            );

            let mut h2 = host(2);
            h2.capabilities = caps_with(
                CapStatus::Ok(None),
                CapStatus::Failed("no stamp".into()),
                CapStatus::NotApplicable,
                CapStatus::NotApplicable,
                CapStatus::NotApplicable,
                None,
            );
            assert_eq!(
                host_meets_capabilities(&h2, &CapabilityRequirements::default()),
                Err("bundle_stamp"),
            );
        }

        /// `Failed` (probe ran, broke) and `Unknown` (never probed,
        /// though `schema >= 1` says it should have been) both fail a
        /// REQUIRED substrate capability.
        #[test]
        fn required_uffd_substrate_rejects_failed_and_unknown() {
            for bad in [CapStatus::Failed("EINVAL".into()), CapStatus::Unknown] {
                let mut h = host(1);
                h.capabilities = caps_with(
                    CapStatus::Ok(None),
                    CapStatus::Ok(None),
                    bad.clone(),
                    CapStatus::Ok(None),
                    CapStatus::Ok(None),
                    None,
                );
                let req = CapabilityRequirements {
                    needs_uffd_substrate: true,
                    fc_snapshot_version: None,
                };
                assert_eq!(
                    host_meets_capabilities(&h, &req),
                    Err("base_shm_tmpfs"),
                    "base_shm_tmpfs={bad:?} must fail a required substrate placement",
                );
            }
        }

        /// ADR 0022: `NotApplicable` on the substrate caps is the
        /// honest report of a File-backend FC host (the UFFD substrate
        /// was never configured, so there's nothing to probe) — it
        /// must PASS a required substrate placement, not fail it like
        /// `Failed`/`Unknown` do. A File-mode fleet must stay
        /// placeable for memory-manifest restores (finding 1,
        /// PR #564 review): it serves them via the File-backend path
        /// instead of UFFD.
        #[test]
        fn required_uffd_substrate_passes_when_not_applicable() {
            let mut h = host(1);
            h.capabilities = caps_with(
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::NotApplicable,
                CapStatus::NotApplicable,
                CapStatus::NotApplicable,
                None,
            );
            let req = CapabilityRequirements {
                needs_uffd_substrate: true,
                fc_snapshot_version: None,
            };
            assert!(
                host_meets_capabilities(&h, &req).is_ok(),
                "a File-mode host (substrate caps NotApplicable) must remain placeable \
                 for memory-manifest sessions"
            );
        }

        #[test]
        fn uffd_substrate_ok_when_all_three_caps_ok() {
            let mut h = host(1);
            h.capabilities = fully_ok();
            let req = CapabilityRequirements {
                needs_uffd_substrate: true,
                fc_snapshot_version: None,
            };
            assert!(host_meets_capabilities(&h, &req).is_ok());
        }

        #[test]
        fn uffd_substrate_not_required_ignores_substrate_caps() {
            let mut h = host(1);
            h.capabilities = caps_with(
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Failed("not mounted".into()),
                CapStatus::Failed("no kernel support".into()),
                CapStatus::Failed("module absent".into()),
                None,
            );
            // needs_uffd_substrate: false (default) — the base gate is
            // all that's checked.
            assert!(host_meets_capabilities(&h, &CapabilityRequirements::default()).is_ok());
        }

        #[test]
        fn fc_snapshot_version_mismatch_rejects() {
            let mut h = host(1);
            h.capabilities = caps_with(
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                Some("v9.0.0"),
            );
            let req = CapabilityRequirements {
                needs_uffd_substrate: false,
                fc_snapshot_version: Some("v10.0.0".to_string()),
            };
            assert_eq!(
                host_meets_capabilities(&h, &req),
                Err("fc_snapshot_version")
            );
        }

        #[test]
        fn fc_snapshot_version_match_passes() {
            let mut h = host(1);
            h.capabilities = caps_with(
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                Some("v10.0.0"),
            );
            let req = CapabilityRequirements {
                needs_uffd_substrate: false,
                fc_snapshot_version: Some("v10.0.0".to_string()),
            };
            assert!(host_meets_capabilities(&h, &req).is_ok());
        }

        /// Either side `None` on the version pairing imposes no
        /// constraint — pre-migration snapshot rows / VZ / a host that
        /// hasn't probed it yet must not be spuriously excluded.
        #[test]
        fn fc_snapshot_version_either_side_none_is_unconstrained() {
            let mut host_no_version = host(1);
            host_no_version.capabilities = fully_ok();
            let req_wants_version = CapabilityRequirements {
                needs_uffd_substrate: false,
                fc_snapshot_version: Some("v10.0.0".to_string()),
            };
            assert!(host_meets_capabilities(&host_no_version, &req_wants_version).is_ok());

            let mut host_with_version = host(2);
            host_with_version.capabilities = caps_with(
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                Some("v10.0.0"),
            );
            assert!(host_meets_capabilities(
                &host_with_version,
                &CapabilityRequirements::default()
            )
            .is_ok());
        }

        /// `host_passes_filters` wires the gate in: a schedulable host
        /// that fails a required capability is excluded from
        /// `rank_hosts`.
        #[test]
        fn rank_hosts_excludes_a_host_failing_required_capabilities() {
            let mut ready = host(1);
            ready.capabilities = fully_ok();
            let mut substrate_broken = host(2);
            substrate_broken.capabilities = caps_with(
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                CapStatus::Failed("not mounted".into()),
                CapStatus::Ok(None),
                CapStatus::Ok(None),
                None,
            );
            let mut c = ctx();
            c.caps = CapabilityRequirements {
                needs_uffd_substrate: true,
                fc_snapshot_version: None,
            };
            let ranked = rank_hosts(&[ready, substrate_broken], &c, Utc::now(), TTL);
            assert_eq!(ranked.hosts, vec![hid(1)]);
        }

        /// ADR 0068 "no capacity with free hosts" mystery-mode killer:
        /// `exclusion_summary` names the FIRST reason each host is
        /// excluded, in the documented precedence order.
        #[test]
        fn exclusion_summary_names_the_first_matching_reason() {
            let mut cordoned = host(1);
            cordoned.cordoned = true;
            let mut skewed = host(2);
            skewed.wire_version = engram_protocol::WIRE_VERSION + 1;
            let mut stale = host(3);
            stale.last_heartbeat_at = Utc::now() - chrono::Duration::hours(1);
            let mut cap_failed = host(4);
            cap_failed.capabilities = fully_ok();
            cap_failed.capabilities.grpc_self_connect = CapStatus::Failed("refused".into());
            let fine = host(5);

            let c = ctx();
            let summary = exclusion_summary(
                &[
                    cordoned.clone(),
                    skewed.clone(),
                    stale.clone(),
                    cap_failed.clone(),
                    fine.clone(),
                ],
                &c,
                Utc::now(),
                TTL,
            );
            assert_eq!(
                summary,
                vec![
                    (hid(1), "cordoned".to_string()),
                    (hid(2), "wire_skew".to_string()),
                    (hid(3), "stale".to_string()),
                    (hid(4), "cap:grpc_self_connect".to_string()),
                    (hid(5), "no_fit".to_string()),
                ],
            );
        }

        #[test]
        fn exclusion_summary_names_the_excluded_host_first() {
            let target = host(1);
            let c = {
                let mut c = ctx();
                c.exclude_host = Some(hid(1));
                c
            };
            let summary = exclusion_summary(&[target], &c, Utc::now(), TTL);
            assert_eq!(summary, vec![(hid(1), "excluded".to_string())]);
        }
    }

    fn ctx<'a>() -> ScheduleContext<'a> {
        ScheduleContext {
            repo: "r",
            image_version: "v",
            snapshot_host: None,
            memory_mib: None,
            cpu_budget_vcpus: None,
            required_image_digest: None,
            exclude_host: None,
            prefer_host: None,
            caps: CapabilityRequirements::default(),
            prefer_bundles: &[],
        }
    }

    /// A reserved-budget map from (host, mem_mib) — vCPU left 0 unless a
    /// test sets it explicitly.
    fn mem_reserved(entries: &[(HostId, i64)]) -> HashMap<HostId, ReservedBudget> {
        entries
            .iter()
            .map(|&(id, mem_mib)| (id, ReservedBudget { mem_mib, vcpus: 0 }))
            .collect()
    }

    const TTL: Duration = Duration::from_secs(60);

    #[test]
    fn cordoned_and_stale_and_draining_hosts_are_filtered() {
        let mut cordoned = host(1);
        cordoned.cordoned = true;
        let mut stale = host(2);
        stale.last_heartbeat_at = Utc::now() - chrono::Duration::seconds(300);
        let mut draining = host(3);
        draining.status = HostStatus::Draining;
        let ok = host(4);
        let ranked = rank_hosts(&[cordoned, stale, draining, ok], &ctx(), Utc::now(), TTL);
        assert_eq!(ranked.hosts, vec![hid(4)]);
    }

    /// ADR 0078 tier 0: an alive, headroom-OK `snapshot_host` is picked
    /// authoritatively — even over a host with more free RAM (h1) that
    /// best-fit would otherwise prefer. The bytes are on h2's NVMe.
    #[test]
    fn authoritative_snapshot_host_wins_when_healthy() {
        let mut h1 = host(1);
        h1.utilization.allocatable_mib = 64_000; // more free than h2
        let mut h2 = host(2);
        h2.utilization.allocatable_mib = 16_000;
        let mut c = ctx();
        c.snapshot_host = Some(hid(2));
        let pick = pick_from(&[h1, h2], &HashMap::new(), &c, Utc::now(), TTL).unwrap();
        assert_eq!(
            pick,
            hid(2),
            "tier-0 places on the host that holds the chunks"
        );
    }

    /// ADR 0078 tier 0 fallbacks: each unusable `snapshot_host` state
    /// falls through to the soft tiers (here: the other live host) —
    /// acceptance criterion #3. (The `reason` label is emitted as a
    /// metric; this asserts the fallback *behaviour*.)
    #[test]
    fn snapshot_host_vetoes_fall_through_to_soft_tiers() {
        // dead: snapshot_host not in the fleet.
        let mut c = ctx();
        c.snapshot_host = Some(hid(9)); // absent
        let pick = pick_from(&[host(1)], &HashMap::new(), &c, Utc::now(), TTL).unwrap();
        assert_eq!(pick, hid(1), "dead snapshot_host → soft fallback");

        // cordoned: present but cordoned → fall through to the other host.
        let mut cordoned = host(2);
        cordoned.cordoned = true;
        let mut c = ctx();
        c.snapshot_host = Some(hid(2));
        let pick = pick_from(&[host(1), cordoned], &HashMap::new(), &c, Utc::now(), TTL).unwrap();
        assert_eq!(pick, hid(1), "cordoned snapshot_host → soft fallback");

        // disk_full: free work_dir below the chunk-cache floor.
        let mut disk_full = host(2);
        disk_full.utilization.disk_total_mib = 200_000;
        disk_full.utilization.disk_used_mib =
            200_000 - (engram_core::types::host::HOST_DISK_CACHE_FLOOR_MIB - 1);
        let mut c = ctx();
        c.snapshot_host = Some(hid(2));
        let pick = pick_from(&[host(1), disk_full], &HashMap::new(), &c, Utc::now(), TTL).unwrap();
        assert_eq!(pick, hid(1), "disk-pressured snapshot_host → soft fallback");

        // ram_full: session budget exceeds the snapshot_host's free RAM.
        let mut ram_full = host(2);
        ram_full.utilization.allocatable_mib = 1_000;
        let other = {
            let mut h = host(1);
            h.utilization.allocatable_mib = 64_000;
            h
        };
        let mut c = ctx();
        c.snapshot_host = Some(hid(2));
        c.memory_mib = Some(8_000); // doesn't fit h2's 1,000 free
        let pick = pick_from(&[other, ram_full], &HashMap::new(), &c, Utc::now(), TTL).unwrap();
        assert_eq!(pick, hid(1), "RAM-full snapshot_host → soft fallback");
    }

    /// ADR 0080 phase 3b: the capture/materialize picker's disk gate —
    /// `host_disk_floor_ok` shares the exact floor + unmeasured-is-soft
    /// posture with `named_host_fit_veto`'s `disk_full` arm, so a host
    /// the resume path would veto for disk can't be handed an
    /// image-sized materialize/capture either.
    #[test]
    fn capture_picker_disk_floor_matches_the_tier0_veto() {
        // Unmeasured disk (dev / brand-new host): soft, no veto.
        assert!(host_disk_floor_ok(&host(1)));

        // One MiB below the floor: vetoed.
        let mut pressured = host(2);
        pressured.utilization.disk_total_mib = 200_000;
        pressured.utilization.disk_used_mib =
            200_000 - (engram_core::types::host::HOST_DISK_CACHE_FLOOR_MIB - 1);
        assert!(!host_disk_floor_ok(&pressured));

        // Exactly at the floor: allowed (>= semantics, same as the
        // named-host veto's `<` reject).
        let mut at_floor = host(3);
        at_floor.utilization.disk_total_mib = 200_000;
        at_floor.utilization.disk_used_mib =
            200_000 - engram_core::types::host::HOST_DISK_CACHE_FLOOR_MIB;
        assert!(host_disk_floor_ok(&at_floor));
    }

    /// ADR 0078 re-review (finding #1): a tier-0 veto must not be
    /// nullified by the tier-2 prefer arm. The resume path passes the
    /// SAME host as both `snapshot_host` and `prefer_host`
    /// (`api/snapshot.rs`: `prefer_host: record.host_id.or(...)`), so the
    /// prefer arm must apply the same disk/RAM/CPU fit gate — otherwise
    /// the disk-pressured host tier-0 just rejected (and counted as a
    /// fallback) is re-picked one arm later.
    #[test]
    fn disk_vetoed_snapshot_host_is_not_repicked_via_prefer() {
        let mut disk_full = host(2);
        disk_full.utilization.disk_total_mib = 200_000;
        disk_full.utilization.disk_used_mib =
            200_000 - (engram_core::types::host::HOST_DISK_CACHE_FLOOR_MIB - 1);
        let mut c = ctx();
        c.snapshot_host = Some(hid(2));
        c.prefer_host = Some(hid(2)); // the resume path's exact shape
        let pick = pick_from(
            &[host(1), disk_full.clone()],
            &HashMap::new(),
            &c,
            Utc::now(),
            TTL,
        )
        .unwrap();
        assert_eq!(
            pick,
            hid(1),
            "a disk-vetoed snapshot host must not sneak back in as prefer_host"
        );

        // The prefer arm applies the gate on its own too (uniform fix):
        // a disk-pressured prefer host with NO snapshot affinity in play
        // falls through the same way.
        let mut c = ctx();
        c.prefer_host = Some(hid(2));
        let pick = pick_from(&[host(1), disk_full], &HashMap::new(), &c, Utc::now(), TTL).unwrap();
        assert_eq!(pick, hid(1), "disk-pressured prefer_host → falls through");

        // A healthy prefer host still wins over row order (the soft
        // preference itself is unchanged — host(2) is unmeasured, so
        // without prefer the fallback would pick host(1)).
        let mut c = ctx();
        c.prefer_host = Some(hid(2));
        let pick = pick_from(&[host(1), host(2)], &HashMap::new(), &c, Utc::now(), TTL).unwrap();
        assert_eq!(pick, hid(2), "a healthy prefer_host is still preferred");
    }

    #[test]
    fn digest_gate_yields_image_not_ready() {
        let mut ready = host(1);
        ready.ready_images.push("sha256:abc".into());
        let unready = host(2);
        let mut c = ctx();
        c.required_image_digest = Some(ManifestDigest::new("sha256:abc"));
        let ranked = rank_hosts(&[ready.clone(), unready.clone()], &c, Utc::now(), TTL);
        assert_eq!(ranked.hosts, vec![hid(1)]);

        c.required_image_digest = Some(ManifestDigest::new("sha256:other"));
        let err = pick_from(&[ready, unready], &HashMap::new(), &c, Utc::now(), TTL).unwrap_err();
        assert!(matches!(err, PickError::ImageNotReady(_)));
    }

    #[test]
    fn best_fit_packs_the_tightest_measured_host() {
        // h1 has LESS free (4000) than h2 (32000); best-fit packs h1.
        let mut h1 = host(1);
        h1.utilization.allocatable_mib = 32_000;
        let mut h2 = host(2);
        h2.utilization.allocatable_mib = 32_000;
        let reserved = mem_reserved(&[(hid(1), 28_000)]);
        let pick = pick_from(&[h1, h2], &reserved, &ctx(), Utc::now(), TTL).unwrap();
        assert_eq!(pick, hid(1), "best-fit packs the tighter host");
    }

    #[test]
    fn parked_resident_on_one_host_never_gets_placed_on() {
        // Issue #540: both hosts have 32,000 MiB physical RAM and zero
        // reservations, but h1 "parks" a 12,288 MiB (12 GiB) resident VM
        // (epic-parking-ladder rungs 2-3) while h2 has nothing resident.
        // The ledger's `allocatable_mib` NEVER adds parked PSS back in
        // (`RamLedgerSnapshot::allocatable_mib`, engram-host-agent), so
        // by the time placement sees these two hosts, h1's allocatable
        // is already 12,288 MiB lower than h2's — exactly as if that RAM
        // were simply unavailable. Placement (which only ever sees the
        // final `allocatable_mib`, never ledger internals) must route a
        // session that needs more than h1's remaining headroom to h2,
        // never double-counting the parked VM's bytes as free on h1.
        let mut h1 = host(1);
        h1.utilization.allocatable_mib = 32_000 - 12_288; // parked VM already excluded
        let mut h2 = host(2);
        h2.utilization.allocatable_mib = 32_000; // nothing resident
        let mut c = ctx();
        c.memory_mib = Some(24_000); // fits h2 only; h1 has 19,712 free
        let pick = pick_from(&[h1, h2], &HashMap::new(), &c, Utc::now(), TTL).unwrap();
        assert_eq!(
            pick,
            hid(2),
            "a session too big for h1's parked-excluded headroom must land on h2, \
             not get double-counted onto the parking host",
        );
    }

    #[test]
    fn cpu_budget_excludes_a_host_with_no_free_vcpu() {
        // h1 is the tighter RAM fit but its CPU budget is exhausted; a
        // 2-vCPU session must land on h2.
        let mut h1 = host(1);
        h1.utilization.allocatable_mib = 32_000;
        h1.total_vcpus = 8; // budget = 8 × overcommit(4) = 32
        let mut h2 = host(2);
        h2.utilization.allocatable_mib = 32_000;
        h2.total_vcpus = 8;
        let mut c = ctx();
        c.memory_mib = Some(4_096);
        c.cpu_budget_vcpus = Some(2);
        // h1 reserved 32 vCPU (full); h2 reserved 0.
        let reserved: HashMap<HostId, ReservedBudget> = [
            (
                hid(1),
                ReservedBudget {
                    mem_mib: 0,
                    vcpus: 32,
                },
            ),
            (
                hid(2),
                ReservedBudget {
                    mem_mib: 0,
                    vcpus: 0,
                },
            ),
        ]
        .into();
        let pick = pick_from(&[h1, h2], &reserved, &c, Utc::now(), TTL).unwrap();
        assert_eq!(pick, hid(2), "CPU-exhausted host is excluded");
    }

    #[test]
    fn prefer_host_wins_when_it_fits_loses_when_full() {
        let mut h1 = host(1);
        h1.utilization.allocatable_mib = 32_000;
        let mut h2 = host(2);
        h2.utilization.allocatable_mib = 8_000;
        let mut c = ctx();
        c.prefer_host = Some(hid(2));
        c.memory_mib = Some(4_096);
        let pick = pick_from(
            &[h1.clone(), h2.clone()],
            &HashMap::new(),
            &c,
            Utc::now(),
            TTL,
        )
        .unwrap();
        assert_eq!(pick, hid(2), "prefer_host fits → wins over best-fit");

        let reserved = mem_reserved(&[(hid(2), 6_000)]);
        let pick = pick_from(&[h1, h2], &reserved, &c, Utc::now(), TTL).unwrap();
        assert_eq!(pick, hid(1), "prefer_host full → falls through");
    }

    #[test]
    fn capacity_soft_fallback_picks_first_ranked() {
        // No host has a measured allocatable → fallback, not NoCapacity
        // (this path is deliberately capacity-soft; only
        // reserve_placement commits).
        let hosts = [host(1), host(2)];
        let mut c = ctx();
        c.memory_mib = Some(4_096);
        let pick = pick_from(&hosts, &HashMap::new(), &c, Utc::now(), TTL).unwrap();
        assert_eq!(pick, hid(1));
    }

    #[test]
    fn exclude_host_is_never_picked() {
        let mut c = ctx();
        c.exclude_host = Some(hid(1));
        let ranked = rank_hosts(&[host(1)], &c, Utc::now(), TTL);
        assert!(ranked.hosts.is_empty());
        assert!(matches!(
            pick_from(&[host(1)], &HashMap::new(), &c, Utc::now(), TTL),
            Err(PickError::NoCapacity)
        ));
    }

    #[test]
    fn wire_version_skewed_host_is_drained_from_scheduling() {
        // Issue #229: a host reporting a NONZERO wire version that differs
        // from the coordinator's is excluded from placement, so a
        // non-atomic rolling deploy drains off stale hosts instead of
        // hard-failing sessions on them. A version of 0 (not yet reported)
        // and the coordinator's own version both stay schedulable.
        let coord = engram_protocol::WIRE_VERSION;

        let mut skewed = host(1);
        skewed.wire_version = coord + 1;
        assert!(
            !host_wire_version_ok(&skewed),
            "a nonzero mismatched version must be excluded",
        );
        assert!(!host_is_schedulable(&skewed, Utc::now(), TTL));

        let mut not_reported = host(2);
        not_reported.wire_version = 0;
        assert!(
            host_wire_version_ok(&not_reported),
            "0 = not yet reported is tolerated (soft posture)",
        );
        assert!(host_is_schedulable(&not_reported, Utc::now(), TTL));

        let mut matched = host(3);
        matched.wire_version = coord;
        assert!(host_wire_version_ok(&matched));
        assert!(host_is_schedulable(&matched, Utc::now(), TTL));

        // The picker routes AWAY from the skewed host onto the matched one
        // — it never returns the skewed host and never errors with a
        // decode-shaped failure.
        let pick = pick_from(
            &[skewed.clone(), matched.clone()],
            &HashMap::new(),
            &ctx(),
            Utc::now(),
            TTL,
        )
        .unwrap();
        assert_eq!(
            pick,
            hid(3),
            "session must be placed on the version-matched host"
        );

        // A fleet of ONLY skewed hosts yields NoCapacity (the scheduler
        // drains them), not a placement onto a host that would 400.
        let err = pick_from(&[skewed], &HashMap::new(), &ctx(), Utc::now(), TTL).unwrap_err();
        assert!(matches!(err, PickError::NoCapacity));
    }

    /// ADR 0090: bundle-covered hosts outrank uncovered ones (soft —
    /// uncovered hosts still rank, last), steering recoveries away from
    /// mid-staging fresh nodes without ever stranding a resume.
    #[test]
    fn rank_hosts_prefers_bundle_covered_hosts_without_excluding() {
        use engram_core::types::sandbox::AuxBundleRef;
        let pin = [AuxBundleRef {
            drive_id: "dyn_0".into(),
            sha256: "abc123".into(),
        }];
        let mut fresh = host(1); // row-order first, but NO stamp coverage
        fresh.current_bundles = vec![];
        let mut staged = host(2);
        staged.current_bundles = vec![AuxBundleRef {
            // different drive-id spelling on purpose: coverage is sha-only
            drive_id: "slot0".into(),
            sha256: "abc123".into(),
        }];
        let mut c = ctx();
        c.prefer_bundles = &pin;
        let ranked = rank_hosts(&[fresh.clone(), staged.clone()], &c, Utc::now(), TTL);
        assert_eq!(
            ranked.hosts,
            vec![staged.id, fresh.id],
            "covered host first, uncovered still present (soft ordering)"
        );
        // Empty prefer set imposes no reordering (row order preserved).
        let ranked = rank_hosts(&[fresh.clone(), staged.clone()], &ctx(), Utc::now(), TTL);
        assert_eq!(ranked.hosts, vec![fresh.id, staged.id]);
    }

    #[test]
    fn fleet_counts_exclude_cordoned_from_schedulable_only() {
        // Pure variant of fleet_snapshot's filter, via host_is_schedulable.
        let mut cordoned = host(1);
        cordoned.cordoned = true;
        assert!(!host_is_schedulable(&cordoned, Utc::now(), TTL));
        assert!(host_is_schedulable(&host(2), Utc::now(), TTL));
    }

    /// 2026-07-11 campaign: a "ready" host whose substrate capability
    /// probe fails must NOT count as autoscaler capacity — counting it
    /// made the scaler complacent while placement excluded the host and
    /// creates queue-timed out. (`schema == 0` = pre-capability row still
    /// passes: soft posture during a mixed-version roll.)
    #[test]
    fn fleet_counts_exclude_capability_failed_hosts() {
        let now = Utc::now();
        let healthy = host(1);
        assert!(host_counts_as_capacity(&healthy, now, TTL));

        let mut cap_failed = host(2);
        cap_failed.capabilities.schema = 1;
        cap_failed.capabilities.grpc_self_connect =
            engram_core::types::host::CapStatus::Failed("refused".into());
        assert!(
            host_is_schedulable(&cap_failed, now, TTL),
            "precondition: the host looks schedulable to the pre-fix filter"
        );
        assert!(!host_counts_as_capacity(&cap_failed, now, TTL));
    }

    mod capture_placement {
        use super::*;

        fn live() -> std::collections::HashSet<HostId> {
            std::collections::HashSet::new()
        }

        #[test]
        fn footprint_math_matches_the_documented_formula() {
            // 2×image + mem + 4096 slack.
            let f = CaptureFootprint::for_capture(1000, 2048);
            assert_eq!(f.disk_mib, 2 * 1000 + 2048 + 4096);
            // ceil(2.5×image) + 1024.
            let f = CaptureFootprint::for_materialize(1001);
            assert_eq!(f.disk_mib, 2503 + 1024);
            assert_eq!(CaptureFootprint::floor_only().disk_mib, 0);
        }

        #[test]
        fn footprint_veto_excludes_a_host_without_headroom() {
            let mut roomy = host(1);
            roomy.utilization.disk_total_mib = 200_000;
            roomy.utilization.disk_used_mib = 100_000; // 100,000 free
            let mut tight = host(2);
            tight.utilization.disk_total_mib = 200_000;
            tight.utilization.disk_used_mib = 190_000; // 10,000 free
            let need = CaptureFootprint { disk_mib: 50_000 };
            let cands =
                capture_candidate_hosts_from(&[roomy, tight], &live(), need, None, Utc::now(), TTL);
            assert_eq!(
                cands,
                vec![hid(1)],
                "the tight host lacks footprint headroom"
            );
        }

        #[test]
        fn footprint_veto_is_soft_for_unmeasured_hosts() {
            // Unmeasured (disk_total_mib == 0) never gets vetoed by
            // footprint, same posture as the disk floor.
            let unmeasured = host(1);
            let need = CaptureFootprint {
                disk_mib: 1_000_000,
            };
            let cands =
                capture_candidate_hosts_from(&[unmeasured], &live(), need, None, Utc::now(), TTL);
            assert_eq!(cands, vec![hid(1)]);
        }

        #[test]
        fn disk_floor_still_applies_under_the_capture_picker() {
            let mut pressured = host(1);
            pressured.utilization.disk_total_mib = 200_000;
            pressured.utilization.disk_used_mib =
                200_000 - (engram_core::types::host::HOST_DISK_CACHE_FLOOR_MIB - 1);
            let cands = capture_candidate_hosts_from(
                &[pressured],
                &live(),
                CaptureFootprint::floor_only(),
                None,
                Utc::now(),
                TTL,
            );
            assert!(cands.is_empty(), "a disk-pressured host is not a candidate");
        }

        #[test]
        fn anti_affinity_excludes_a_host_already_running_a_capture_job() {
            let free1 = host(1);
            let free2 = host(2);
            let mut busy: std::collections::HashSet<HostId> = std::collections::HashSet::new();
            busy.insert(hid(1));
            let cands = capture_candidate_hosts_from(
                &[free1, free2],
                &busy,
                CaptureFootprint::floor_only(),
                None,
                Utc::now(),
                TTL,
            );
            assert_eq!(
                cands,
                vec![hid(2)],
                "host 1 already runs a live capture job"
            );
        }

        #[test]
        fn anti_affinity_alone_can_exhaust_the_fleet() {
            let mut busy: std::collections::HashSet<HostId> = std::collections::HashSet::new();
            busy.insert(hid(1));
            let cands = capture_candidate_hosts_from(
                &[host(1)],
                &busy,
                CaptureFootprint::floor_only(),
                None,
                Utc::now(),
                TTL,
            );
            assert!(cands.is_empty());
        }

        #[test]
        fn fc_snapshot_version_pin_excludes_a_mismatched_host() {
            let mut wrong = host(1);
            wrong.capabilities = engram_core::types::host::HostCapabilities {
                schema: 1,
                backend: "firecracker".into(),
                grpc_self_connect: engram_core::types::host::CapStatus::Ok(None),
                base_shm_tmpfs: engram_core::types::host::CapStatus::Ok(None),
                uffd_minor_shmem: engram_core::types::host::CapStatus::Ok(None),
                nbd: engram_core::types::host::CapStatus::Ok(None),
                bundle_stamp: engram_core::types::host::CapStatus::Ok(None),
                fc_snapshot_version: Some("v9.0.0".into()),
                wire_version: 7,
            };
            let mut right = wrong.clone();
            right.id = hid(2);
            right.capabilities.fc_snapshot_version = Some("v10.0.0".into());
            let cands = capture_candidate_hosts_from(
                &[wrong, right],
                &live(),
                CaptureFootprint::floor_only(),
                Some("v10.0.0"),
                Utc::now(),
                TTL,
            );
            assert_eq!(cands, vec![hid(2)]);
        }

        #[test]
        fn max_free_disk_ranking_orders_candidates_roomiest_first() {
            // Row order would put host 1 first; the ranking must put host 2
            // (more free disk) first so the 2D fit inherits that order.
            let mut small = host(1);
            small.utilization.disk_total_mib = 400_000;
            small.utilization.disk_used_mib = 300_000; // 100,000 free
            let mut roomy = host(2);
            roomy.utilization.disk_total_mib = 400_000;
            roomy.utilization.disk_used_mib = 100_000; // 300,000 free
            let cands = capture_candidate_hosts_from(
                &[small, roomy],
                &live(),
                CaptureFootprint::floor_only(),
                None,
                Utc::now(),
                TTL,
            );
            assert_eq!(
                cands.first(),
                Some(&hid(2)),
                "must rank by max free disk, not row order"
            );
            assert!(cands.contains(&hid(1)), "both hosts are still candidates");
        }

        #[test]
        fn ties_among_equal_or_unmeasured_hosts_break_by_row_order() {
            let h1 = host(1);
            let h2 = host(2);
            let cands = capture_candidate_hosts_from(
                &[h1, h2],
                &live(),
                CaptureFootprint::floor_only(),
                None,
                Utc::now(),
                TTL,
            );
            assert_eq!(cands, vec![hid(1), hid(2)]);
        }
    }
}
