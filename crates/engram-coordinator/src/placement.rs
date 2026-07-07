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
    for h in hosts {
        if host_passes_filters(h, ctx, now, ttl) {
            ranked.push(h.id);
        }
    }
    RankedCandidates {
        hosts: ranked,
        affinity_len: 0,
    }
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
    // 4. capacity-soft fallback.
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
) -> Result<bool, PickError> {
    let (hosts, reserved) = hosts_and_reserved(meta).await?;
    let ranked = rank_hosts(&hosts, ctx, Utc::now(), placement_ttl());
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
) -> Result<RankedCandidates, PickError> {
    let hosts = meta
        .list_active_hosts()
        .await
        .map_err(|e| PickError::Internal(format!("list_active_hosts: {e}")))?;
    Ok(rank_hosts(&hosts, ctx, Utc::now(), placement_ttl()))
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
) {
    let hosts = match meta.list_active_hosts().await {
        Ok(hosts) => hosts,
        Err(e) => {
            tracing::debug!(origin, error = %e,
                "log_empty_candidates: list_active_hosts failed; skipping exclusion visibility");
            return;
        }
    };
    let summary = exclusion_summary(&hosts, ctx, Utc::now(), placement_ttl());
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

/// Session scheduler for the resume/evac path: pick from the hosts rows
/// and resolve the backend (dialing through the PG `host_addr` when this
/// replica hasn't seen the host yet). Emits the ADR 0044 K4
/// demand-pressure counter on every decision.
pub async fn pick_for_session(
    meta: &dyn MetadataStore,
    registry: &HostRegistry,
    ctx: &ScheduleContext<'_>,
) -> Result<(HostId, Arc<dyn HostClient>), PickError> {
    let result = pick_for_session_inner(meta, registry, ctx).await;
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
        log_empty_candidates(meta, ctx, "resume").await;
    }
    result
}

async fn pick_for_session_inner(
    meta: &dyn MetadataStore,
    registry: &HostRegistry,
    ctx: &ScheduleContext<'_>,
) -> Result<(HostId, Arc<dyn HostClient>), PickError> {
    let (hosts, reserved) = hosts_and_reserved(meta).await?;
    let id = pick_from(&hosts, &reserved, ctx, Utc::now(), placement_ttl())?;
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
) -> Result<(HostId, Arc<dyn HostClient>), PickError> {
    if Some(host_id) == exclude_host {
        return Err(PickError::NoCapacity);
    }
    let (hosts, reserved) = hosts_and_reserved(meta).await?;
    let now = Utc::now();
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

/// ADR 0020 P1: any schedulable host for a base-snapshot capture or an
/// ADR 0080 image materialize — NOT gated on image readiness or RAM/CPU
/// capacity (the capture host lazy-materializes the rootfs from
/// BlobStorage), but it IS gated on the ADR 0078 tier-0 disk floor:
/// both jobs write image-sized data under the host's work dir, so a
/// host already below the chunk-cache floor (about to disk-evict its
/// cache) must never be handed more disk work. Fixed here for both
/// consumers (the old capture picker ignored disk entirely — the
/// `capture-host picker ignores disk` incident class).
pub async fn pick_capture_host(
    meta: &dyn MetadataStore,
    registry: &HostRegistry,
) -> Result<(HostId, Arc<dyn HostClient>), PickError> {
    let hosts = meta
        .list_active_hosts()
        .await
        .map_err(|e| PickError::Internal(format!("list_active_hosts: {e}")))?;
    let now = Utc::now();
    let ttl = placement_ttl();
    let id = hosts
        .iter()
        .find(|h| {
            host_is_schedulable(h, now, ttl)
                // ADR 0068: base gate only — see `pick_specific_host`.
                && host_meets_capabilities(h, &CapabilityRequirements::default()).is_ok()
                // ADR 0078 tier-0 disk veto (same floor + same
                // unmeasured-is-soft posture as `named_host_fit_veto`).
                && host_disk_floor_ok(h)
        })
        .map(|h| h.id)
        .ok_or(PickError::NoCapacity)?;
    let backend = registry
        .backend_for(id)
        .await
        .map_err(|e| PickError::HostUnreachable(id, e.to_string()))?;
    Ok((id, backend))
}

/// ADR 0078's tier-0 disk floor as a standalone predicate: free
/// work_dir space at or above [`engram_core::types::host::HOST_DISK_CACHE_FLOOR_MIB`].
/// Unmeasured (`disk_total_mib == 0`) is soft — no veto, the same
/// posture as unmeasured RAM (brand-new / dev hosts). Kept in lockstep
/// with the disk arm of [`named_host_fit_veto`].
fn host_disk_floor_ok(h: &HostRecord) -> bool {
    if h.utilization.disk_total_mib == 0 {
        return true;
    }
    let free_disk_mib = h
        .utilization
        .disk_total_mib
        .saturating_sub(h.utilization.disk_used_mib);
    free_disk_mib >= engram_core::types::host::HOST_DISK_CACHE_FLOOR_MIB
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
) -> Result<(HostId, SandboxId), SandboxError> {
    let (host_id, backend) = pick_for_session(meta, registry, ctx).await?;
    let sandbox_id = backend.restore(metadata, fence).await?;
    registry.record_sandbox_owner(sandbox_id, host_id);
    Ok((host_id, sandbox_id))
}

/// Fleet-wide autoscaling counts (ADR 0044 K4 + ADR 0048 CPU dims),
/// from the hosts rows + the per-host reserved aggregate.
pub async fn fleet_snapshot(meta: &dyn MetadataStore) -> Result<FleetSnapshot, PickError> {
    let hosts = meta
        .list_active_hosts()
        .await
        .map_err(|e| PickError::Internal(format!("list_active_hosts: {e}")))?;
    let reserved = meta
        .per_host_reserved()
        .await
        .map_err(|e| PickError::Internal(format!("per_host_reserved: {e}")))?;
    let now = Utc::now();
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
        if host_is_schedulable(h, now, ttl) {
            m.schedulable_hosts += 1;
            m.total_mib += h.utilization.allocatable_mib;
            let cpu_budget = engram_core::types::host::host_cpu_budget(h.total_vcpus);
            m.total_vcpus += cpu_budget as u64;
            let reserved_vcpus = reserved.get(&h.id).map(|r| r.vcpus).unwrap_or(0);
            m.free_vcpus += (cpu_budget - reserved_vcpus).max(0) as u64;
        }
    }
    Ok(m)
}

#[cfg(test)]
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
            cordoned: false,
            total_vcpus: 0,
            // Issue #229: 0 = "not yet reported" → tolerated by the
            // placement filter. Tests that exercise the skew gate set this
            // to a concrete version explicitly.
            wire_version: 0,
            stages_images: false,
            capabilities: engram_core::types::host::HostCapabilities::default(),
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

    #[test]
    fn fleet_counts_exclude_cordoned_from_schedulable_only() {
        // Pure variant of fleet_snapshot's filter, via host_is_schedulable.
        let mut cordoned = host(1);
        cordoned.cordoned = true;
        assert!(!host_is_schedulable(&cordoned, Utc::now(), TTL));
        assert!(host_is_schedulable(&host(2), Utc::now(), TTL));
    }
}
