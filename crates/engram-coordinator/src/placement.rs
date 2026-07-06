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
use engram_core::traits::{HostClient, MetadataStore};
use engram_core::types::host::{HostRecord, HostStatus, ReservedBudget};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{HostId, SandboxError, SandboxId, SnapshotId};
use engram_protocol::heartbeat::ManifestDigest;

use crate::host_registry::HostRegistry;

/// Inputs the scheduler considers when picking a host. (Moved verbatim
/// from `host_registry` by ADR 0047 — the semantics are unchanged, only
/// the backing store moved from the in-memory mirror to the hosts rows.)
#[derive(Clone, Debug)]
pub struct ScheduleContext<'a> {
    pub repo: &'a str,
    pub image_version: &'a str,
    /// Snapshot id to prefer (zero-cost hot tier hit). `None` for fresh
    /// sessions; `Some` when resuming or migrating.
    pub prefer_snapshot_id: Option<SnapshotId>,
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
            PickError::HostUnreachable(h, msg) => {
                SandboxError::Vm(format!("picked host {h} is unreachable: {msg}").into())
            }
            PickError::Internal(msg) => SandboxError::Vm(format!("placement read: {msg}").into()),
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
    match ctx.required_image_digest.as_ref() {
        Some(d) => h.ready_images.iter().any(|r| r == d.as_str()),
        None => true,
    }
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
    let mut ranked: Vec<HostId> = Vec::new();
    if let Some(target) = ctx.prefer_snapshot_id {
        for h in hosts {
            if host_passes_filters(h, ctx, now, ttl)
                && h.local_snapshots.iter().any(|s| s.snapshot_id == target)
            {
                ranked.push(h.id);
            }
        }
    }
    let affinity_len = ranked.len();
    for h in hosts {
        if host_passes_filters(h, ctx, now, ttl) && !ranked.contains(&h.id) {
            ranked.push(h.id);
        }
    }
    RankedCandidates {
        hosts: ranked,
        affinity_len,
    }
}

/// Pure pick for the resume/evac path. Ranking tiers:
///
/// 1. snapshot-affinity (capacity-blind — the hot-tier hit is worth it),
/// 2. `prefer_host` if it fits both dimensions,
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
    // 1. snapshot-affinity prefix.
    if ranked.affinity_len > 0 {
        return Ok(ranked.hosts[0]);
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
    // 2. soft host-affinity, when it fits both dims (unknown RAM counts
    //    as fitting — same posture as `choose_placement_host`).
    if let Some(want) = ctx.prefer_host {
        if ranked.hosts.contains(&want)
            && free_mib_of(want).is_none_or(|f| f >= need_mib)
            && cpu_fits(want)
        {
            return Ok(want);
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

/// ADR 0020 P1: any schedulable host for a base-snapshot capture — NOT
/// gated on image readiness or capacity (the capture host
/// lazy-materializes the rootfs from BlobStorage).
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
        .find(|h| host_is_schedulable(h, now, ttl))
        .map(|h| h.id)
        .ok_or(PickError::NoCapacity)?;
    let backend = registry
        .backend_for(id)
        .await
        .map_err(|e| PickError::HostUnreachable(id, e.to_string()))?;
    Ok((id, backend))
}

/// Pick a host for `ctx`, then `restore` from `metadata` on it. (The
/// pre-0047 `HostRegistry::restore_for_session`, relocated.) Caller is
/// responsible for `assign_session_host`.
#[tracing::instrument(name = "coord.restore_for_session", skip_all)]
pub async fn restore_for_session(
    meta: &dyn MetadataStore,
    registry: &HostRegistry,
    ctx: &ScheduleContext<'_>,
    metadata: SnapshotMetadata,
) -> Result<(HostId, SandboxId), SandboxError> {
    let (host_id, backend) = pick_for_session(meta, registry, ctx).await?;
    let sandbox_id = backend.restore(metadata).await?;
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
    use engram_core::types::host::{
        HostCapacity, HostLocalSnapshot, HostMetadata, HostUtilization,
    };
    use engram_core::SessionId;

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
            local_snapshots: Vec::new(),
            current_bundles: Vec::new(),
            cordoned: false,
            total_vcpus: 0,
            // Issue #229: 0 = "not yet reported" → tolerated by the
            // placement filter. Tests that exercise the skew gate set this
            // to a concrete version explicitly.
            wire_version: 0,
        }
    }

    fn hid(id: u128) -> HostId {
        HostId(uuid::Uuid::from_u128(id))
    }

    fn ctx<'a>() -> ScheduleContext<'a> {
        ScheduleContext {
            repo: "r",
            image_version: "v",
            prefer_snapshot_id: None,
            memory_mib: None,
            cpu_budget_vcpus: None,
            required_image_digest: None,
            exclude_host: None,
            prefer_host: None,
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

    #[test]
    fn snapshot_affinity_hosts_form_the_prefix() {
        let snap = SnapshotId(uuid::Uuid::from_u128(99));
        let mut with_snap = host(2);
        with_snap.local_snapshots.push(HostLocalSnapshot {
            snapshot_id: snap,
            session_id: SessionId(uuid::Uuid::from_u128(1)),
            size_bytes: 1,
            replicated: true,
            last_accessed_at: Utc::now(),
        });
        let hosts = [host(1), with_snap];
        let mut c = ctx();
        c.prefer_snapshot_id = Some(snap);
        let ranked = rank_hosts(&hosts, &c, Utc::now(), TTL);
        assert_eq!(ranked.affinity_len, 1);
        assert_eq!(ranked.hosts, vec![hid(2), hid(1)]);
        // And the pick takes the affinity host even though h1 has more
        // measured free RAM.
        let pick = pick_from(&hosts, &HashMap::new(), &c, Utc::now(), TTL).unwrap();
        assert_eq!(pick, hid(2));
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
