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
use engram_core::types::host::{HostRecord, HostStatus};
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

/// A host the scheduler may place on: host-reported `ready` (a
/// `draining` status is the agent's own shutdown flag), not
/// coordinator-cordoned, and heartbeat-fresh within `ttl`.
pub fn host_is_schedulable(h: &HostRecord, now: DateTime<Utc>, ttl: Duration) -> bool {
    h.status == HostStatus::Ready
        && !h.cordoned
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

/// Pure pick for the resume/evac path. Ranking tiers (unchanged from the
/// pre-0047 in-memory picker):
///
/// 1. snapshot-affinity (capacity-blind — the hot-tier hit is worth it),
/// 2. `prefer_host` if it fits,
/// 3. largest free RAM among hosts that fit `memory_mib`,
/// 4. fallback: the first ranked candidate (hosts without an
///    allocatable measurement yet, or nothing fits — this path is
///    deliberately capacity-soft; only `reserve_placement` commits).
///
/// "free" is `allocatable_mib − reserved` (ADR 0046's real headroom)
/// rather than the old phantom `total − used(=0)`.
pub fn pick_from(
    hosts: &[HostRecord],
    reserved_mib: &HashMap<HostId, i64>,
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
    let free_of = |id: HostId| -> Option<i64> {
        let h = hosts.iter().find(|h| h.id == id)?;
        let alloc = h.utilization.allocatable_mib as i64;
        if alloc <= 0 {
            return None; // no measurement yet — unknown, not zero
        }
        Some(alloc - reserved_mib.get(&id).copied().unwrap_or(0))
    };
    let need = ctx.memory_mib.unwrap_or(0) as i64;
    // 2. soft host-affinity, when it fits (unknown allocatable counts
    //    as fitting — same posture as `choose_placement_host`).
    if let Some(want) = ctx.prefer_host {
        if ranked.hosts.contains(&want) && free_of(want).is_none_or(|f| f >= need) {
            return Ok(want);
        }
    }
    // 3. largest measured free that fits.
    let mut best: Option<(i64, HostId)> = None;
    for &id in &ranked.hosts {
        let Some(free) = free_of(id) else { continue };
        if free < need {
            continue;
        }
        match best {
            Some((bf, _)) if bf >= free => {}
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
) -> Result<(Vec<HostRecord>, HashMap<HostId, i64>), PickError> {
    let hosts = meta
        .list_active_hosts()
        .await
        .map_err(|e| PickError::Internal(format!("list_active_hosts: {e}")))?;
    let reserved = meta
        .per_host_reserved_mib()
        .await
        .map_err(|e| PickError::Internal(format!("per_host_reserved_mib: {e}")))?;
    Ok((hosts, reserved))
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
    if alloc > 0 && alloc - reserved_of(&reserved, host_id) <= 0 {
        return Err(PickError::NoCapacity);
    }
    let backend = registry
        .backend_for(host_id)
        .await
        .map_err(|e| PickError::HostUnreachable(host_id, e.to_string()))?;
    Ok((host_id, backend))
}

fn reserved_of(reserved: &HashMap<HostId, i64>, id: HostId) -> i64 {
    reserved.get(&id).copied().unwrap_or(0)
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

/// Fleet-wide autoscaling counts (ADR 0044 K4), from the hosts rows.
pub async fn fleet_snapshot(meta: &dyn MetadataStore) -> Result<FleetSnapshot, PickError> {
    let hosts = meta
        .list_active_hosts()
        .await
        .map_err(|e| PickError::Internal(format!("list_active_hosts: {e}")))?;
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
        if host_is_schedulable(h, now, ttl) {
            m.schedulable_hosts += 1;
            m.total_mib += h.utilization.allocatable_mib;
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
            required_image_digest: None,
            exclude_host: None,
            prefer_host: None,
        }
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
    fn largest_measured_free_wins_and_reserved_counts() {
        let mut h1 = host(1);
        h1.utilization.allocatable_mib = 32_000;
        let mut h2 = host(2);
        h2.utilization.allocatable_mib = 32_000;
        let reserved: HashMap<HostId, i64> = [(hid(1), 28_000i64)].into();
        let pick = pick_from(&[h1, h2], &reserved, &ctx(), Utc::now(), TTL).unwrap();
        assert_eq!(pick, hid(2));
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
        assert_eq!(pick, hid(2), "prefer_host fits → wins over larger free");

        let reserved: HashMap<HostId, i64> = [(hid(2), 6_000i64)].into();
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
    fn fleet_counts_exclude_cordoned_from_schedulable_only() {
        // Pure variant of fleet_snapshot's filter, via host_is_schedulable.
        let mut cordoned = host(1);
        cordoned.cordoned = true;
        assert!(!host_is_schedulable(&cordoned, Utc::now(), TTL));
        assert!(host_is_schedulable(&host(2), Utc::now(), TTL));
    }
}
