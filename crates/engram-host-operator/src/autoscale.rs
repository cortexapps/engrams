//! ADR 0048: the STATELESS scale-down wave executor.
//!
//! The wave is recomputed from scratch every reconcile — no in-memory wave
//! object. Its only durable state is a Node annotation,
//! `fleet.engram.io/scaledown-victim: <fleet>`, written when a victim is
//! cordoned. That annotation (a) distinguishes a wave-cordon from a manual
//! one, (b) survives an operator restart so a half-finished wave resumes, and
//! (c) is the set the executor reads to know what's in flight.
//!
//! Each reconcile, [`step`] reads coordinator demand + the per-host load and
//! decides one [`StepAction`] ([`plan_step`], pure + unit-tested):
//!
//! - **Queue pressure or scale-up** ⇒ ABORT the wave: uncordon + de-annotate
//!   every not-yet-removed victim (instant capacity return), then grow the
//!   pool if needed. A burst mid-wave reclaims the cordoned nodes immediately.
//! - **Scale-down** (queue empty, hysteresis met, image roll quiescent) ⇒
//!   START a wave: [`plan_wave`](crate::wave::plan_wave) picks victims, each is
//!   cordoned + annotated, then up to `maxConcurrentDrains` are driven
//!   `drain → gate(running→0) → remove_node → delete_host`. A drain that
//!   times out uncordons + de-annotates that victim and the others continue.
//! - An **in-flight** wave (annotated victims present) is CONTINUED every tick
//!   regardless of hysteresis, and blocks new image rolls until it finishes.
//! - Otherwise **hold**.
//!
//! A timed-out image roll is a separate, durable availability-debt state
//! (`fleet.engram.io/roll-stuck`, ADR 0044/0048 amendments). Its node still
//! exists physically but cannot accept placement, so queued demand targets
//! `desired_schedulable + stuck_rolls`. Once replacement capacity is Ready
//! and the queue is empty, the same drain-gated named-node removal used by a
//! scale-down wave retires the stuck node. A transiently joining node carries
//! no marker and therefore never compounds scale-up.
//!
//! The actuation rides two seams — [`CoordApi`] (coordinator admin calls) and
//! [`NodeOps`] (K8s node cordon + annotation) — so the executor is exercised
//! against recording mocks without a cluster or a coordinator.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use engram_core::traits::cloud::NodePoolScaler;
use engram_core::HostId;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;

use crate::coord::{CoordClient, HostLoad, HostStatus};
use crate::crd::HostFleetSpec;
use crate::error::OperatorError;
use crate::reconcile::PodInfo;
use crate::reconcile::ROLL_STUCK_ANNOTATION;
use crate::scaler::{desired_hosts, AutoscalePolicy};
use crate::wave::{plan_wave, WaveHost, WavePolicy};

/// Node annotation marking a host as the victim of an in-flight scale-down
/// wave for fleet `<value>`. Written at cordon time; removed on abort or once
/// the node is gone. Its presence == "this cordon is a wave-cordon".
pub const VICTIM_ANNOTATION: &str = "fleet.engram.io/scaledown-victim";

/// Coordinator admin calls the wave drives. A trait so the executor runs
/// against a recording mock in tests (the live impl is [`CoordClient`]).
#[async_trait]
pub trait CoordApi: Send + Sync {
    async fn fleet_demand(&self) -> Result<crate::scaler::FleetDemand, OperatorError>;
    async fn list_hosts(&self) -> Result<Vec<HostLoad>, OperatorError>;
    async fn host_status(&self, host: HostId) -> Result<Option<HostStatus>, OperatorError>;
    async fn cordon(&self, host: HostId) -> Result<(), OperatorError>;
    async fn uncordon(&self, host: HostId) -> Result<(), OperatorError>;
    async fn drain(&self, host: HostId) -> Result<(), OperatorError>;
    async fn delete_host(&self, host: HostId) -> Result<(), OperatorError>;
}

#[async_trait]
impl CoordApi for CoordClient {
    async fn fleet_demand(&self) -> Result<crate::scaler::FleetDemand, OperatorError> {
        CoordClient::fleet_demand(self).await
    }
    async fn list_hosts(&self) -> Result<Vec<HostLoad>, OperatorError> {
        CoordClient::list_hosts(self).await
    }
    async fn host_status(&self, host: HostId) -> Result<Option<HostStatus>, OperatorError> {
        CoordClient::host_status(self, host).await
    }
    async fn cordon(&self, host: HostId) -> Result<(), OperatorError> {
        CoordClient::cordon(self, host).await
    }
    async fn uncordon(&self, host: HostId) -> Result<(), OperatorError> {
        CoordClient::uncordon(self, host).await
    }
    async fn drain(&self, host: HostId) -> Result<(), OperatorError> {
        CoordClient::drain(self, host).await
    }
    async fn delete_host(&self, host: HostId) -> Result<(), OperatorError> {
        CoordClient::delete_host(self, host).await
    }
}

/// K8s node operations the wave needs: cordon (unschedulable) + the victim
/// annotation. A trait for the same test-seam reason as [`CoordApi`].
#[async_trait]
pub trait NodeOps: Send + Sync {
    async fn set_unschedulable(&self, node: &str, val: bool) -> Result<(), OperatorError>;
    /// Set (Some) or clear (None) the victim annotation on `node`.
    async fn set_victim(&self, node: &str, fleet: Option<&str>) -> Result<(), OperatorError>;
    /// Every node currently annotated as a victim of `fleet`.
    async fn annotated_victims(&self, fleet: &str) -> Result<Vec<String>, OperatorError>;
    /// Clear the durable timed-out-image-roll marker on `node` after the
    /// named node has been safely removed.
    async fn clear_roll_stuck(&self, node: &str) -> Result<(), OperatorError>;

    /// The K8s Node object's `creationTimestamp`, for the node-ready
    /// bring-up histogram. Defaulted to `None` so the recording test
    /// mocks don't have to model it — a missing Node just skips the
    /// sample.
    async fn node_created_at(
        &self,
        _node: &str,
    ) -> Result<Option<std::time::SystemTime>, OperatorError> {
        Ok(None)
    }
}

/// Live K8s implementation of [`NodeOps`].
pub struct K8sNodeOps<'a> {
    pub client: Client,
    /// One label-scoped, cacheable snapshot shared by all node reads in this
    /// reconcile. Writes still go directly to the API server.
    pub managed_nodes: &'a [Node],
}

#[async_trait]
impl NodeOps for K8sNodeOps<'_> {
    async fn set_unschedulable(&self, node: &str, val: bool) -> Result<(), OperatorError> {
        let nodes: Api<Node> = Api::all(self.client.clone());
        let patch = serde_json::json!({ "spec": { "unschedulable": val } });
        nodes
            .patch(node, &PatchParams::default(), &Patch::Merge(patch))
            .await?;
        Ok(())
    }

    async fn set_victim(&self, node: &str, fleet: Option<&str>) -> Result<(), OperatorError> {
        let nodes: Api<Node> = Api::all(self.client.clone());
        // A `null` annotation value deletes the key under a merge patch.
        let val = match fleet {
            Some(f) => serde_json::Value::String(f.to_string()),
            None => serde_json::Value::Null,
        };
        let patch = serde_json::json!({
            "metadata": { "annotations": { VICTIM_ANNOTATION: val } }
        });
        nodes
            .patch(node, &PatchParams::default(), &Patch::Merge(patch))
            .await?;
        Ok(())
    }

    async fn annotated_victims(&self, fleet: &str) -> Result<Vec<String>, OperatorError> {
        use kube::ResourceExt;
        Ok(self
            .managed_nodes
            .iter()
            .filter(|n| {
                n.annotations()
                    .get(VICTIM_ANNOTATION)
                    .map(|v| v == fleet)
                    .unwrap_or(false)
            })
            .map(|n| n.name_any())
            .collect())
    }

    async fn clear_roll_stuck(&self, node: &str) -> Result<(), OperatorError> {
        let nodes: Api<Node> = Api::all(self.client.clone());
        let patch = serde_json::json!({
            "metadata": { "annotations": { ROLL_STUCK_ANNOTATION: serde_json::Value::Null } }
        });
        nodes
            .patch(node, &PatchParams::default(), &Patch::Merge(patch))
            .await?;
        Ok(())
    }

    async fn node_created_at(
        &self,
        node: &str,
    ) -> Result<Option<std::time::SystemTime>, OperatorError> {
        Ok(self
            .managed_nodes
            .iter()
            .find(|n| n.metadata.name.as_deref() == Some(node))
            .and_then(|n| n.metadata.creation_timestamp.clone())
            .map(|t| t.0.into()))
    }
}

/// Cross-tick memory for the node-ready histogram: the set of host ids
/// already observed registered with the coordinator. `None` until the
/// first observation — the first tick seeds the set WITHOUT emitting, so
/// an operator restart never reports pre-existing hosts as fresh joins.
#[derive(Default)]
pub struct NodeReadyTracker(std::sync::Mutex<Option<std::collections::HashSet<HostId>>>);

impl NodeReadyTracker {
    /// One observation of the registered-host set. Returns the hosts that
    /// are new since the previous observation — EMPTY on the very first
    /// call, which seeds the set instead (so an operator restart never
    /// reports pre-existing hosts as fresh joins). Pure state transition,
    /// split out from the async emission for direct unit testing.
    fn observe(&self, registered: std::collections::HashSet<HostId>) -> Vec<HostId> {
        let mut guard = self.0.lock().expect("node-ready tracker poisoned");
        match guard.as_mut() {
            None => {
                *guard = Some(registered);
                Vec::new()
            }
            Some(seen) => {
                let fresh = registered.difference(seen).copied().collect();
                *seen = registered;
                fresh
            }
        }
    }
}

/// Minute-based scale-down hysteresis, independent of reconcile frequency.
///
/// Scale-up needs a short reconcile interval, but making the controller poll
/// faster must not also make scale-down more aggressive. The CRD keeps its
/// existing `scaleDownHysteresisTicks` surface; one tick is explicitly one
/// minute instead of one reconcile. The first observation counts as tick 1,
/// matching the old 60-second reconcile behavior.
#[derive(Default)]
pub struct ScaleDownHysteresis(std::sync::Mutex<ScaleDownHysteresisState>);

#[derive(Default)]
struct ScaleDownHysteresisState {
    ticks: u32,
    last_tick: Option<tokio::time::Instant>,
}

const SCALE_DOWN_HYSTERESIS_TICK: Duration = Duration::from_secs(60);

impl ScaleDownHysteresis {
    fn observe(&self, target: bool) -> u32 {
        self.observe_at(target, crate::time_source::metrics_now_tokio())
    }

    fn observe_at(&self, target: bool, now: tokio::time::Instant) -> u32 {
        let mut state = self.0.lock().expect("scale-down hysteresis poisoned");
        if !target {
            *state = ScaleDownHysteresisState::default();
            return 0;
        }

        match state.last_tick {
            None => {
                state.ticks = 1;
                state.last_tick = Some(now);
            }
            Some(last) => {
                if now.duration_since(last) >= SCALE_DOWN_HYSTERESIS_TICK {
                    state.ticks = state.ticks.saturating_add(1);
                    state.last_tick = Some(now);
                }
            }
        }
        state.ticks
    }
}

/// Emit `engram_node_ready_seconds` for every host newly visible in the
/// coordinator's list: K8s Node creation → registration, the whole
/// bring-up pipeline (instance create + kubelet join + assets staging +
/// register). Best-effort — a missing Node object skips the sample.
async fn track_node_ready(
    tracker: &NodeReadyTracker,
    nodes: &dyn NodeOps,
    pods: &[PodInfo],
    hosts: &[HostLoad],
) {
    // The lock is confined to `observe` — never held across the async
    // Node lookups below.
    let fresh = tracker.observe(hosts.iter().map(|h| h.id).collect());
    for host in fresh {
        let Some(pod) = pods
            .iter()
            .find(|p| HostId::from_node_name(&p.node) == host)
        else {
            continue;
        };
        match nodes.node_created_at(&pod.node).await {
            Ok(Some(created)) => {
                if let Ok(age) = crate::time_source::metrics_wall_now().duration_since(created) {
                    ::metrics::histogram!(crate::metrics::NODE_READY_SECONDS)
                        .record(age.as_secs_f64());
                    tracing::info!(node = %pod.node, %host, age_secs = age.as_secs(),
                        "node bring-up complete: host registered");
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(node = %pod.node, error = %e,
                    "node-ready sample skipped: Node lookup failed");
            }
        }
    }
}

/// What [`step`] decided to do this reconcile. Pure output of [`plan_step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepAction {
    /// Queue/scale-up pressure: return any cordoned wave capacity, then grow
    /// (the caller actuates `set_size` only if `desired > physical`).
    AbortAndGrow,
    /// Queue pressure with no grow needed: just return cordoned capacity.
    AbortOnly,
    /// Begin a fresh scale-down wave (hysteresis met + image roll quiescent).
    StartWave,
    /// Continue an in-flight wave (finish what's started — ignores hysteresis
    /// and the roll gate; an in-flight wave already blocks rolls).
    ContinueWave,
    /// Nothing to do.
    Hold,
}

/// Inputs to the pure step decision.
#[derive(Clone, Copy, Debug)]
pub struct StepInputs {
    pub desired: u32,
    /// Grow-only cloud target after adding durable unavailable-capacity debt.
    pub grow_target: u32,
    pub current: u32,
    pub physical: u32,
    pub queued_sessions: u64,
    pub scale_down_enabled: bool,
    pub hysteresis_ready: bool,
    pub roll_idle: bool,
    pub wave_in_flight: bool,
}

/// The pure wave-step decision — see [`StepAction`]. Precedence: queue/scale-up
/// pressure aborts everything; otherwise an in-flight wave continues; a fresh
/// wave needs hysteresis + a quiescent roll; else hold.
pub fn plan_step(i: StepInputs) -> StepAction {
    let scale_up = i.grow_target > i.physical;
    if i.queued_sessions > 0 || scale_up {
        return if scale_up {
            StepAction::AbortAndGrow
        } else {
            StepAction::AbortOnly
        };
    }
    if !i.scale_down_enabled {
        return StepAction::Hold;
    }
    // An in-flight wave is finished regardless of the scale-down target — once
    // victims are cordoned + draining, see them through.
    if i.wave_in_flight {
        return StepAction::ContinueWave;
    }
    if i.desired < i.current && i.hysteresis_ready && i.roll_idle {
        return StepAction::StartWave;
    }
    StepAction::Hold
}

/// Result of one autoscale step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoscaleStatus {
    /// A scale-down wave is draining victims, or stuck-roll remediation
    /// mutated the fleet this tick. The reconcile loop must not start an
    /// image roll and should requeue soon.
    ///
    /// Deliberately NOT set for queue/scale-up pressure or for mere
    /// stuck-roll debt (issue #1012): a roll is a reattach pod swap that
    /// removes no capacity, one-roll-at-a-time is enforced by `plan_roll`'s
    /// not-Ready arm, and the floor gate protects capacity. A queue starved
    /// by wire skew is served BY the roll — blocking on it deadlocks the
    /// fleet (the queue waits for the roll, the roll waits for the queue).
    pub blocks_roll: bool,
}

/// K8s/roll state observed by one stateless autoscale reconcile.
pub struct FleetObservation<'a> {
    pub pods: &'a [PodInfo],
    pub roll_idle: bool,
    pub stuck_rolls: &'a [String],
}

/// Bundle of the actuation seams + wave knobs, passed to the drive helpers.
pub struct WaveActuator<'a> {
    pub coord: &'a dyn CoordApi,
    pub nodes: &'a dyn NodeOps,
    pub scaler: &'a dyn NodePoolScaler,
    pub fleet: &'a str,
    pub node_pool: &'a str,
    pub max_concurrent_drains: u32,
    pub drain_timeout: Duration,
}

/// Map each pod's node name → its coordinator [`HostLoad`] (joined by the
/// deterministic `HostId::from_node_name`), building the [`WaveHost`] snapshot
/// the planner consumes. Nodes with no matching coord host (just joined,
/// already gone) are skipped. `annotated` marks the in-flight victims `pinned`.
pub fn assemble_wave_hosts(
    pods: &[PodInfo],
    coord_hosts: &[HostLoad],
    annotated: &[String],
) -> Vec<WaveHost> {
    let by_id: HashMap<HostId, &HostLoad> = coord_hosts.iter().map(|h| (h.id, h)).collect();
    pods.iter()
        .filter(|p| !p.node.is_empty())
        .filter_map(|p| {
            let host = by_id.get(&HostId::from_node_name(&p.node))?;
            let pinned = annotated.iter().any(|n| n == &p.node);
            // A host cordoned by something OTHER than this wave (a manual
            // operator cordon, or an image roll) is off-limits: don't pick it
            // as a fresh victim, and don't count its capacity as a survivor's
            // (it isn't accepting placements). Our own in-flight victims are
            // cordoned + annotated → they pass through as `pinned`.
            if host.cordoned && !pinned {
                return None;
            }
            Some(WaveHost {
                node: p.node.clone(),
                running_sandboxes: host.running_sandboxes,
                reserved_mib: host.reserved_mib,
                reserved_vcpus: host.reserved_vcpus,
                free_mib: host.free_mib,
                free_vcpus: host.free_vcpus,
                pinned,
            })
        })
        .collect()
}

/// Uncordon + de-annotate every in-flight victim — the wave abort path.
/// Best-effort per node (a single failure is logged, the rest proceed) so a
/// transient error can't strand capacity cordoned.
async fn abort_wave(act: &WaveActuator<'_>, victims: &[String]) {
    for node in victims {
        let host = HostId::from_node_name(node);
        let r = async {
            act.coord.uncordon(host).await?;
            act.nodes.set_unschedulable(node, false).await?;
            act.nodes.set_victim(node, None).await?;
            Ok::<(), OperatorError>(())
        }
        .await;
        match r {
            Ok(()) => {
                ::metrics::counter!(
                    crate::metrics::AUTOSCALE_VICTIMS_RELEASED_TOTAL, "reason" => "abort"
                )
                .increment(1);
                tracing::info!(%node, %host, "wave abort: uncordoned + de-annotated victim")
            }
            Err(e) => {
                tracing::warn!(%node, %host, error=%e, "wave abort: failed to release victim")
            }
        }
    }
}

/// Drive the planned victims. Cordons + annotates every victim first (stops new
/// placement on doomed nodes), then drives up to `max_concurrent_drains`
/// through `drain → gate → remove_node → delete_host`. Returns how many victims
/// remain in flight (cordoned, not yet removed) — non-zero means "wave still
/// running".
async fn drive_victims(act: &WaveActuator<'_>, victims: &[String]) -> u32 {
    // 1. Cordon + annotate all victims (idempotent — a re-driven in-flight
    //    victim is already cordoned).
    for node in victims {
        let host = HostId::from_node_name(node);
        if let Err(e) = ensure_cordoned(act, node, host).await {
            tracing::warn!(%node, error=%e, "wave: failed to cordon victim; will retry next tick");
        }
    }

    // 2. Drive up to max_concurrent_drains victims to full removal this tick.
    let mut in_flight = victims.len() as u32;
    let budget = act.max_concurrent_drains.max(1);
    for node in victims.iter().take(budget as usize) {
        let host = HostId::from_node_name(node);
        match drive_one(act, node, host).await {
            DriveOutcome::Removed => in_flight = in_flight.saturating_sub(1),
            DriveOutcome::Released => in_flight = in_flight.saturating_sub(1),
            DriveOutcome::StillDraining => {}
        }
    }
    in_flight
}

enum DriveOutcome {
    /// Drained + removed + deregistered.
    Removed,
    /// Drain timed out → uncordoned + de-annotated (capacity returned).
    Released,
    /// Transient error mid-pipeline — left cordoned for the next tick.
    StillDraining,
}

/// Cordon a victim on both the coordinator (durable, stops placement) and the
/// K8s node (stops stray scheduling), and annotate it as a wave victim.
async fn ensure_cordoned(
    act: &WaveActuator<'_>,
    node: &str,
    host: HostId,
) -> Result<(), OperatorError> {
    act.coord.cordon(host).await?;
    act.nodes.set_unschedulable(node, true).await?;
    act.nodes.set_victim(node, Some(act.fleet)).await?;
    Ok(())
}

/// One victim through `drain → gate → remove_node → delete_host`. A drain
/// timeout releases the victim (uncordon + de-annotate) so it rejoins the
/// fleet — never strand a session because one host wouldn't drain.
async fn drive_one(act: &WaveActuator<'_>, node: &str, host: HostId) -> DriveOutcome {
    if let Err(e) = act.coord.drain(host).await {
        tracing::warn!(%node, %host, error=%e, "wave: drain call failed; retry next tick");
        return DriveOutcome::StillDraining;
    }
    match gate_drain(act.coord, host, act.drain_timeout).await {
        Ok(()) => {}
        Err(OperatorError::DrainTimeout { remaining, .. }) => {
            tracing::warn!(
                %node, %host, remaining,
                "wave: drain timed out — releasing victim (uncordon + de-annotate)"
            );
            let _ = act.coord.uncordon(host).await;
            let _ = act.nodes.set_unschedulable(node, false).await;
            let _ = act.nodes.set_victim(node, None).await;
            ::metrics::counter!(
                crate::metrics::AUTOSCALE_VICTIMS_RELEASED_TOTAL, "reason" => "drain_timeout"
            )
            .increment(1);
            return DriveOutcome::Released;
        }
        Err(e) => {
            tracing::warn!(%node, %host, error=%e, "wave: drain gate errored; retry next tick");
            return DriveOutcome::StillDraining;
        }
    }
    // Drained to 0 sandboxes. Remove the node (decrements MIG target) then
    // deregister the coord row so it doesn't linger to the dead-host TTL.
    if let Err(e) = act.scaler.remove_node(act.node_pool, node).await {
        tracing::warn!(%node, error=%e, "wave: remove_node failed; retry next tick");
        return DriveOutcome::StillDraining;
    }
    if let Err(e) = act.coord.delete_host(host).await {
        // The node is already gone from the cloud; the coord row will fall to
        // the dead-host TTL. Log + move on (don't re-add the node).
        tracing::warn!(%node, %host, error=%e, "wave: delete_host failed; row will TTL out");
    }
    // Clear the annotation defensively (the Node object usually vanishes with
    // the instance, but a slow kubelet deregistration could leave it).
    let _ = act.nodes.set_victim(node, None).await;
    ::metrics::counter!(crate::metrics::AUTOSCALE_VICTIMS_REMOVED_TOTAL).increment(1);
    tracing::info!(%node, %host, "wave: victim drained + removed + deregistered");
    DriveOutcome::Removed
}

/// Retire durable image-roll failures after surge capacity is schedulable.
/// This deliberately does NOT share the scale-down wave's timeout-release
/// arm: a roll-stuck node has no viable host-agent successor, so uncordoning
/// it would advertise broken capacity. Any drain/RPC failure leaves it
/// cordoned + marked for a later retry.
async fn repair_stuck_rolls(act: &WaveActuator<'_>, stuck: &[String]) -> u32 {
    let mut remaining = stuck.len() as u32;
    for node in stuck.iter().take(act.max_concurrent_drains as usize) {
        if repair_stuck_roll(act, node).await {
            remaining = remaining.saturating_sub(1);
        }
    }
    remaining
}

async fn repair_stuck_roll(act: &WaveActuator<'_>, node: &str) -> bool {
    let host = HostId::from_node_name(node);
    if let Err(e) = act.coord.cordon(host).await {
        tracing::warn!(%node, %host, error=%e, "stuck-roll repair: coordinator cordon failed");
        return false;
    }
    if let Err(e) = act.nodes.set_unschedulable(node, true).await {
        tracing::warn!(%node, %host, error=%e, "stuck-roll repair: K8s cordon failed");
        return false;
    }
    if let Err(e) = act.coord.drain(host).await {
        tracing::warn!(%node, %host, error=%e, "stuck-roll repair: drain call failed");
        return false;
    }
    if let Err(e) = gate_drain(act.coord, host, act.drain_timeout).await {
        tracing::warn!(
            %node,
            %host,
            error=%e,
            "stuck-roll repair: drain gate did not complete; keeping node cordoned"
        );
        return false;
    }
    if let Err(e) = act.scaler.remove_node(act.node_pool, node).await {
        tracing::warn!(%node, %host, error=%e, "stuck-roll repair: remove_node failed");
        return false;
    }
    if let Err(e) = act.coord.delete_host(host).await {
        tracing::warn!(%node, %host, error=%e, "stuck-roll repair: delete_host failed; row will TTL out");
    }
    // The cloud removal normally deletes the Node object. Clear the marker
    // best-effort for a slow kubelet deregistration; a NotFound is harmless.
    let _ = act.nodes.clear_roll_stuck(node).await;
    ::metrics::counter!(crate::metrics::AUTOSCALE_STUCK_ROLL_REPAIRS_TOTAL).increment(1);
    tracing::info!(%node, %host, "stuck-roll repair: drained + removed + deregistered");
    true
}

/// Poll the coordinator's drain gate until the host reports
/// `running_sandboxes == 0` AND no in-flight enable work (or it's
/// deregistered), or the budget elapses.
///
/// ADR 0088: the sandbox count *incidentally* covers a capture VM (it is
/// registered in the backend and counted) but a materialize boots no VM —
/// without the `has_enable_work` leg, a scale-down could remove the node
/// under a live materialize. Node removal genuinely destroys the work, so
/// unlike the image roll's proceed-on-timeout gate, this stays inside the
/// wave's existing release-the-victim-on-timeout semantics.
async fn gate_drain(
    coord: &dyn CoordApi,
    host: HostId,
    budget: Duration,
) -> Result<(), OperatorError> {
    let deadline = crate::time_source::metrics_now_tokio() + budget;
    loop {
        match coord.host_status(host).await? {
            None => return Ok(()), // already deregistered
            Some(st) if st.running_sandboxes == 0 && !st.has_enable_work() => return Ok(()),
            Some(st) => {
                if crate::time_source::metrics_now_tokio() >= deadline {
                    return Err(OperatorError::DrainTimeout {
                        host_id: host.to_string(),
                        remaining: st.running_sandboxes,
                    });
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// One autoscale step for the reconcile loop. `roll_idle` is true iff the image
/// roll is `UpToDate` (a fresh wave only starts when no roll is in flight).
/// Returns whether a wave is in flight (the caller blocks rolls + requeues
/// soon while one is).
pub async fn step(
    spec: &HostFleetSpec,
    scaler: &dyn NodePoolScaler,
    scaledown_hysteresis: &ScaleDownHysteresis,
    node_ready: &NodeReadyTracker,
    coord: &dyn CoordApi,
    nodes: &dyn NodeOps,
    observation: FleetObservation<'_>,
) -> Result<AutoscaleStatus, OperatorError> {
    let Some(a) = &spec.autoscaling else {
        return Ok(AutoscaleStatus::default());
    };
    let policy = AutoscalePolicy {
        min_hosts: a.min_hosts,
        max_hosts: a.max_hosts,
        target_free_mib: a.target_free_mib,
        scale_down: a.scale_down,
    };
    let demand = coord.fleet_demand().await?;
    let desired = desired_hosts(demand, policy);
    let current = demand.schedulable_hosts;
    let pods = observation.pods;
    let stuck_rolls = observation.stuck_rolls;
    let physical = pods.len() as u32;
    // ADR 0044/0048 amendment: a timed-out image roll still exists in the
    // managed pool and in the DaemonSet list, but cannot become schedulable.
    // Add that durable debt to the schedulable target. A normal joining node
    // has no marker, so repeated reconciles keep reasserting one idempotent
    // target instead of ratcheting the pool upward.
    let unavailable = stuck_rolls.len() as u32;
    let grow_target = desired
        .saturating_add(unavailable)
        .min(a.max_hosts.max(a.min_hosts));
    // The annotation value keys wave-victims to THIS node pool (distinguishing
    // them from a manual cordon, and from another fleet's wave).
    let fleet_key = a.node_pool.as_str();
    let annotated = nodes.annotated_victims(fleet_key).await?;
    let wave_in_flight = !annotated.is_empty();

    // Fetched once per tick: the node-ready tracker consumes it here and
    // the wave arm below reuses it (it used to fetch its own copy).
    let hosts = coord.list_hosts().await?;
    track_node_ready(node_ready, nodes, pods, &hosts).await;

    for (kind, v) in [
        ("desired", desired),
        ("schedulable", current),
        ("physical", physical),
        ("grow_target", grow_target),
    ] {
        ::metrics::gauge!(crate::metrics::AUTOSCALE_HOSTS, "kind" => kind).set(v as f64);
    }
    ::metrics::gauge!(crate::metrics::AUTOSCALE_WAVE_IN_FLIGHT).set(if wave_in_flight {
        1.0
    } else {
        0.0
    });

    // Hysteresis: accumulate only while a fresh scale-down target persists.
    let scale_down_target = a.scale_down.enabled()
        && desired < current
        && demand.queued_sessions == 0
        && stuck_rolls.is_empty()
        && desired <= physical;
    let hysteresis_ticks = scaledown_hysteresis.observe(scale_down_target && !wave_in_flight);
    let hysteresis_ready = hysteresis_ticks >= a.scale_down_hysteresis_ticks.max(1);

    let action = plan_step(StepInputs {
        desired,
        grow_target,
        current,
        physical,
        queued_sessions: demand.queued_sessions,
        scale_down_enabled: a.scale_down.enabled(),
        hysteresis_ready,
        roll_idle: observation.roll_idle,
        wave_in_flight,
    });
    tracing::info!(
        node_pool = %a.node_pool, current, physical, desired, grow_target,
        unavailable, queued = demand.queued_sessions, wave_in_flight, ?action,
        "autoscale step"
    );
    let action_label = match action {
        StepAction::Hold => "hold",
        StepAction::StartWave => "start_wave",
        StepAction::ContinueWave => "continue_wave",
        StepAction::AbortAndGrow => "abort_and_grow",
        StepAction::AbortOnly => "abort_only",
    };
    ::metrics::counter!(crate::metrics::AUTOSCALE_STEP_ACTIONS_TOTAL, "action" => action_label)
        .increment(1);

    let act = WaveActuator {
        coord,
        nodes,
        scaler,
        fleet: fleet_key,
        node_pool: &a.node_pool,
        max_concurrent_drains: a.max_concurrent_drains.max(1),
        drain_timeout: Duration::from_secs(spec.drain_timeout_seconds),
    };

    let pressure = matches!(action, StepAction::AbortAndGrow | StepAction::AbortOnly);
    if (pressure || !stuck_rolls.is_empty()) && wave_in_flight {
        abort_wave(&act, &annotated).await;
    }
    if grow_target > physical {
        scaler.set_size(&a.node_pool, grow_target).await?;
        ::metrics::counter!(crate::metrics::AUTOSCALE_GROWS_TOTAL).increment(1);
        tracing::info!(
            node_pool=%a.node_pool,
            desired,
            grow_target,
            physical,
            unavailable,
            "autoscale: grew pool (including unavailable-capacity debt)"
        );
    }

    if !stuck_rolls.is_empty() {
        // Do not compete with queued creates for the replacement capacity.
        // Once the queue drains and the schedulable target is actually met,
        // safely drain + remove the named broken node.
        if demand.queued_sessions == 0 && current >= desired {
            let remaining = repair_stuck_rolls(&act, stuck_rolls).await;
            tracing::info!(
                node_pool=%a.node_pool,
                stuck = stuck_rolls.len(),
                remaining,
                "stuck-roll repair step"
            );
            // Repair may have removed a named node this tick — block the
            // precomputed image-roll decision; the next reconcile must
            // observe the new fleet first.
            return Ok(AutoscaleStatus { blocks_roll: true });
        }
        // Durable debt alone never blocks a roll (issue #1012): the stuck
        // node's successor is not Ready, so `plan_roll` already yields
        // WaitForReady, and when the queue is starved by wire skew the roll
        // is the only thing that can serve it.
        return Ok(AutoscaleStatus::default());
    }

    match action {
        StepAction::AbortAndGrow | StepAction::AbortOnly => {
            // Queue/scale-up pressure aborts waves and grows the pool, but
            // never suppresses image rolls (issue #1012): a roll removes no
            // capacity (reattach pod swap, floor-gated), and a queue starved
            // by wire skew is served BY the roll — blocking here deadlocks
            // (queue waits for roll, roll waits for queue).
            Ok(AutoscaleStatus::default())
        }
        StepAction::Hold => Ok(AutoscaleStatus::default()),
        StepAction::StartWave | StepAction::ContinueWave => {
            let wave_hosts = assemble_wave_hosts(pods, &hosts, &annotated);
            let wpolicy = WavePolicy {
                mode: a.scale_down,
                max_shed_per_wave: a.max_shed_per_wave.max(1),
                floor: a.min_hosts.max(spec.capacity_floor),
                headroom_mib: a.target_free_mib,
            };
            let decision = plan_wave(&wave_hosts, desired, wpolicy);
            tracing::info!(node_pool=%a.node_pool, victims=?decision.victims, note=%decision.note, "wave plan");
            if decision.victims.is_empty() {
                // Nothing safe to shed this tick; clear any leftover in-flight
                // annotations so a stuck wave doesn't block rolls forever.
                if wave_in_flight {
                    abort_wave(&act, &annotated).await;
                }
                return Ok(AutoscaleStatus::default());
            }
            let in_flight = drive_victims(&act, &decision.victims).await;
            Ok(AutoscaleStatus {
                blocks_roll: in_flight > 0,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn inputs() -> StepInputs {
        StepInputs {
            desired: 5,
            grow_target: 5,
            current: 5,
            physical: 5,
            queued_sessions: 0,
            scale_down_enabled: true,
            hysteresis_ready: true,
            roll_idle: true,
            wave_in_flight: false,
        }
    }

    #[test]
    fn node_ready_tracker_seeds_silently_then_reports_fresh() {
        let t = NodeReadyTracker::default();
        let a = HostId::from_node_name("gke-engrams-kvm-a");
        let b = HostId::from_node_name("gke-engrams-kvm-b");
        // First observation seeds without reporting (operator restart must
        // not emit stale bring-up samples for pre-existing hosts).
        assert!(t.observe([a].into_iter().collect()).is_empty());
        // A host new since the seed is reported exactly once.
        assert_eq!(t.observe([a, b].into_iter().collect()), vec![b]);
        assert!(t.observe([a, b].into_iter().collect()).is_empty());
    }

    #[test]
    fn node_ready_tracker_reports_rejoin_after_departure() {
        let t = NodeReadyTracker::default();
        let a = HostId::from_node_name("gke-engrams-kvm-a");
        let b = HostId::from_node_name("gke-engrams-kvm-b");
        assert!(t.observe([a, b].into_iter().collect()).is_empty());
        // b departs (scale-down)…
        assert!(t.observe([a].into_iter().collect()).is_empty());
        // …and a same-named node rejoining is a fresh bring-up again.
        assert_eq!(t.observe([a, b].into_iter().collect()), vec![b]);
    }

    #[test]
    fn scale_down_hysteresis_uses_minute_ticks_not_reconcile_count() {
        let h = ScaleDownHysteresis::default();
        let start = crate::time_source::metrics_now_tokio();

        assert_eq!(h.observe_at(true, start), 1);
        assert_eq!(h.observe_at(true, start + Duration::from_secs(10)), 1);
        assert_eq!(h.observe_at(true, start + Duration::from_secs(59)), 1);
        assert_eq!(h.observe_at(true, start + Duration::from_secs(60)), 2);
        assert_eq!(h.observe_at(true, start + Duration::from_secs(180)), 3);

        assert_eq!(h.observe_at(false, start + Duration::from_secs(181)), 0);
        assert_eq!(h.observe_at(true, start + Duration::from_secs(182)), 1);
    }

    #[test]
    fn queue_pressure_aborts_even_mid_wave() {
        let i = StepInputs {
            queued_sessions: 3,
            wave_in_flight: true,
            desired: 5,
            grow_target: 5,
            physical: 5,
            ..inputs()
        };
        assert_eq!(plan_step(i), StepAction::AbortOnly);
    }

    #[test]
    fn scale_up_aborts_and_grows() {
        let i = StepInputs {
            desired: 8,
            grow_target: 8,
            physical: 5,
            wave_in_flight: true,
            ..inputs()
        };
        assert_eq!(plan_step(i), StepAction::AbortAndGrow);
    }

    #[test]
    fn in_flight_wave_continues_regardless_of_hysteresis_and_roll() {
        let i = StepInputs {
            desired: 3,
            grow_target: 3,
            current: 5,
            wave_in_flight: true,
            hysteresis_ready: false,
            roll_idle: false,
            ..inputs()
        };
        assert_eq!(plan_step(i), StepAction::ContinueWave);
    }

    #[test]
    fn fresh_wave_needs_hysteresis_and_a_quiescent_roll() {
        let base = StepInputs {
            desired: 3,
            grow_target: 3,
            current: 5,
            wave_in_flight: false,
            ..inputs()
        };
        assert_eq!(plan_step(base), StepAction::StartWave);
        assert_eq!(
            plan_step(StepInputs {
                hysteresis_ready: false,
                ..base
            }),
            StepAction::Hold
        );
        assert_eq!(
            plan_step(StepInputs {
                roll_idle: false,
                ..base
            }),
            StepAction::Hold
        );
    }

    #[test]
    fn scale_down_off_holds() {
        let i = StepInputs {
            desired: 3,
            grow_target: 3,
            current: 5,
            scale_down_enabled: false,
            ..inputs()
        };
        assert_eq!(plan_step(i), StepAction::Hold);
    }

    #[test]
    fn at_target_holds() {
        assert_eq!(plan_step(inputs()), StepAction::Hold);
    }

    #[test]
    fn unavailable_debt_grows_past_a_phantom_physical_host() {
        let i = StepInputs {
            desired: 3,
            grow_target: 4,
            current: 2,
            physical: 3,
            queued_sessions: 1,
            ..inputs()
        };
        assert_eq!(plan_step(i), StepAction::AbortAndGrow);
    }

    #[test]
    fn ordinary_joining_capacity_does_not_compound_growth() {
        let i = StepInputs {
            desired: 3,
            grow_target: 3,
            current: 2,
            physical: 3,
            queued_sessions: 0,
            ..inputs()
        };
        assert_eq!(plan_step(i), StepAction::Hold);
    }

    // ---- Recording-mock actuation tests ----

    #[derive(Default)]
    struct Rec {
        log: Mutex<Vec<String>>,
        demand: Mutex<crate::scaler::FleetDemand>,
        /// host_id → running_sandboxes returned by host_status (one entry per
        /// call, popped front; empty → 0).
        drain_progress: Mutex<HashMap<HostId, std::collections::VecDeque<u32>>>,
        /// ADR 0088: host_id → (live_materializes, live_capture_jobs) per
        /// host_status call (popped front; empty → (0, 0)).
        enable_work: Mutex<HashMap<HostId, std::collections::VecDeque<(u32, u32)>>>,
        annotated: Mutex<Vec<String>>,
        /// Returned by `list_hosts` (the wave planner's join input).
        hosts: Mutex<Vec<HostLoad>>,
        /// When set, `remove_node` fails — keeps a wave victim in flight.
        fail_remove: Mutex<bool>,
    }
    impl Rec {
        fn push(&self, s: impl Into<String>) {
            self.log.lock().unwrap().push(s.into());
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CoordApi for Arc<Rec> {
        async fn fleet_demand(&self) -> Result<crate::scaler::FleetDemand, OperatorError> {
            Ok(*self.demand.lock().unwrap())
        }
        async fn list_hosts(&self) -> Result<Vec<HostLoad>, OperatorError> {
            Ok(self.hosts.lock().unwrap().clone())
        }
        async fn host_status(&self, host: HostId) -> Result<Option<HostStatus>, OperatorError> {
            let running = self
                .drain_progress
                .lock()
                .unwrap()
                .get_mut(&host)
                .and_then(|q| q.pop_front())
                .unwrap_or(0);
            let (mats, caps) = self
                .enable_work
                .lock()
                .unwrap()
                .get_mut(&host)
                .and_then(|q| q.pop_front())
                .unwrap_or((0, 0));
            Ok(Some(HostStatus {
                status: "draining".into(),
                running_sandboxes: running,
                live_materializes: mats,
                live_capture_jobs: caps,
            }))
        }
        async fn cordon(&self, host: HostId) -> Result<(), OperatorError> {
            self.push(format!("cordon {host}"));
            Ok(())
        }
        async fn uncordon(&self, host: HostId) -> Result<(), OperatorError> {
            self.push(format!("uncordon {host}"));
            Ok(())
        }
        async fn drain(&self, host: HostId) -> Result<(), OperatorError> {
            self.push(format!("drain {host}"));
            Ok(())
        }
        async fn delete_host(&self, host: HostId) -> Result<(), OperatorError> {
            self.push(format!("delete_host {host}"));
            Ok(())
        }
    }

    #[async_trait]
    impl NodeOps for Arc<Rec> {
        async fn set_unschedulable(&self, node: &str, val: bool) -> Result<(), OperatorError> {
            self.push(format!("unschedulable {node}={val}"));
            Ok(())
        }
        async fn set_victim(&self, node: &str, fleet: Option<&str>) -> Result<(), OperatorError> {
            self.push(format!("victim {node}={fleet:?}"));
            let mut a = self.annotated.lock().unwrap();
            match fleet {
                Some(_) => {
                    if !a.iter().any(|n| n == node) {
                        a.push(node.to_string());
                    }
                }
                None => a.retain(|n| n != node),
            }
            Ok(())
        }
        async fn annotated_victims(&self, _fleet: &str) -> Result<Vec<String>, OperatorError> {
            Ok(self.annotated.lock().unwrap().clone())
        }
        async fn clear_roll_stuck(&self, node: &str) -> Result<(), OperatorError> {
            self.push(format!("roll_stuck {node}=None"));
            Ok(())
        }
    }

    struct RecScaler {
        rec: Arc<Rec>,
    }
    #[async_trait]
    impl NodePoolScaler for RecScaler {
        async fn set_size(
            &self,
            _pool: &str,
            desired: u32,
        ) -> Result<(), engram_core::BackendError> {
            self.rec.push(format!("set_size {desired}"));
            Ok(())
        }
        async fn remove_node(
            &self,
            _pool: &str,
            node: &str,
        ) -> Result<(), engram_core::BackendError> {
            if *self.rec.fail_remove.lock().unwrap() {
                return Err(engram_core::BackendError::Protocol(format!(
                    "injected remove_node failure for {node}"
                )));
            }
            self.rec.push(format!("remove_node {node}"));
            Ok(())
        }
    }

    fn actuator<'a>(rec: &'a Arc<Rec>, scaler: &'a RecScaler, drains: u32) -> WaveActuator<'a> {
        WaveActuator {
            coord: rec,
            nodes: rec,
            scaler,
            fleet: "f",
            node_pool: "kvm",
            max_concurrent_drains: drains,
            drain_timeout: Duration::from_millis(50),
        }
    }

    fn autoscale_spec() -> HostFleetSpec {
        HostFleetSpec {
            daemon_set: crate::crd::DaemonSetRef {
                namespace: "engrams-hosts".into(),
                name: "hf-host-agent".into(),
            },
            image: "host:new".into(),
            node_assets_image: "assets:new".into(),
            coordinator_url: "http://coord".into(),
            capacity_floor: 2,
            drain_timeout_seconds: 1,
            enable_work_timeout_seconds: 0,
            autoscaling: Some(crate::crd::AutoscalingSpec {
                node_pool: "kvm".into(),
                min_hosts: 2,
                max_hosts: 6,
                target_free_mib: 24_576,
                scale_down: crate::scaler::ScaleDownMode::Off,
                scale_down_hysteresis_ticks: 3,
                max_shed_per_wave: 1,
                max_concurrent_drains: 1,
            }),
        }
    }

    fn ready_pod(node: &str) -> PodInfo {
        PodInfo {
            name: format!("pod-{node}"),
            node: node.into(),
            host_image: "host:new".into(),
            init_image: "assets:new".into(),
            ready: true,
        }
    }

    #[tokio::test]
    async fn queued_demand_surges_past_a_roll_stuck_physical_node() {
        let rec = Arc::new(Rec::default());
        *rec.demand.lock().unwrap() = crate::scaler::FleetDemand {
            schedulable_hosts: 2,
            free_mib: 16_713,
            total_mib: 131_072,
            free_vcpus: 80,
            total_vcpus: 80,
            queued_sessions: 1,
            queued_mib: 24_576,
            queued_vcpus: 8,
        };
        let scaler = RecScaler { rec: rec.clone() };
        let hysteresis = ScaleDownHysteresis::default();
        let pods = vec![ready_pod("a"), ready_pod("b"), ready_pod("stuck")];

        let status = step(
            &autoscale_spec(),
            &scaler,
            &hysteresis,
            &NodeReadyTracker::default(),
            &rec,
            &rec,
            FleetObservation {
                pods: &pods,
                roll_idle: false,
                stuck_rolls: &["stuck".into()],
            },
        )
        .await
        .expect("autoscale step");

        assert!(
            !status.blocks_roll,
            "queue pressure / stuck debt must not block image rolls (issue #1012)"
        );
        let log = rec.log();
        assert!(
            log.iter().any(|line| line == "set_size 4"),
            "desired 3 schedulable + 1 stuck debt must surge to 4: {log:?}"
        );
        assert!(
            !log.iter().any(|line| line.starts_with("drain ")),
            "queued creates own replacement capacity before repair: {log:?}"
        );
    }

    /// Issue #1012 regression: after a coord-first wire bump every host is
    /// skew-excluded from placement, sessions queue, and the roll is the only
    /// cure for the skew. Queue pressure (`AbortOnly` — nothing to grow) must
    /// not block the roll, or the fleet deadlocks: the queue waits for the
    /// roll, the roll waits for the queue.
    #[tokio::test]
    async fn skew_starved_queue_does_not_block_the_roll() {
        let rec = Arc::new(Rec::default());
        // Coordinator view mid-deploy: both hosts wire-skewed, so zero
        // schedulable capacity and two queued creates that cannot place.
        *rec.demand.lock().unwrap() = crate::scaler::FleetDemand {
            schedulable_hosts: 0,
            free_mib: 0,
            total_mib: 0,
            free_vcpus: 0,
            total_vcpus: 0,
            queued_sessions: 2,
            queued_mib: 49_152,
            queued_vcpus: 16,
        };
        let scaler = RecScaler { rec: rec.clone() };
        let hysteresis = ScaleDownHysteresis::default();
        // Both pods Ready but running stale images (they need the roll).
        let stale = |node: &str| PodInfo {
            host_image: "host:old".into(),
            init_image: "assets:old".into(),
            ..ready_pod(node)
        };
        let pods = vec![stale("a"), stale("b")];

        let status = step(
            &autoscale_spec(),
            &scaler,
            &hysteresis,
            &NodeReadyTracker::default(),
            &rec,
            &rec,
            FleetObservation {
                pods: &pods,
                roll_idle: false,
                stuck_rolls: &[],
            },
        )
        .await
        .expect("autoscale step");

        assert!(
            !status.blocks_roll,
            "a skew-starved queue must be served BY the roll, never block it"
        );
    }

    /// The one thing that still blocks rolls: an in-flight scale-down wave
    /// (its drains consume the receiving capacity a roll would also need).
    #[tokio::test]
    async fn in_flight_wave_still_blocks_the_roll() {
        let rec = Arc::new(Rec::default());
        // Fleet of 3 with enough free RAM that the cost-optimal target is 2 —
        // the annotated victim's wave continues this tick.
        *rec.demand.lock().unwrap() = crate::scaler::FleetDemand {
            schedulable_hosts: 3,
            free_mib: 180_000,
            total_mib: 196_608,
            free_vcpus: 100,
            total_vcpus: 120,
            ..crate::scaler::FleetDemand::default()
        };
        let load = |node: &str, cordoned: bool| HostLoad {
            id: HostId::from_node_name(node),
            cordoned,
            running_sandboxes: 0,
            reserved_mib: 0,
            free_mib: 60_000,
            reserved_vcpus: 0,
            free_vcpus: 30,
        };
        *rec.hosts.lock().unwrap() = vec![
            load("a", false),
            load("b", false),
            load("victim", true), // cordoned by the wave, pinned via annotation
        ];
        rec.annotated.lock().unwrap().push("victim".into());
        // Transient removal failure keeps the victim in flight this tick.
        *rec.fail_remove.lock().unwrap() = true;
        let mut spec = autoscale_spec();
        spec.autoscaling.as_mut().unwrap().scale_down = crate::scaler::ScaleDownMode::IdleOnly;
        let scaler = RecScaler { rec: rec.clone() };
        let hysteresis = ScaleDownHysteresis::default();
        let pods = vec![ready_pod("a"), ready_pod("b"), ready_pod("victim")];

        let status = step(
            &spec,
            &scaler,
            &hysteresis,
            &NodeReadyTracker::default(),
            &rec,
            &rec,
            FleetObservation {
                pods: &pods,
                roll_idle: true,
                stuck_rolls: &[],
            },
        )
        .await
        .expect("autoscale step");

        assert!(
            status.blocks_roll,
            "an in-flight wave's drains must still block image rolls"
        );
    }

    #[tokio::test]
    async fn roll_stuck_node_is_drain_removed_after_capacity_recovers() {
        let rec = Arc::new(Rec::default());
        *rec.demand.lock().unwrap() = crate::scaler::FleetDemand {
            schedulable_hosts: 3,
            free_mib: 24_576,
            total_mib: 196_608,
            free_vcpus: 80,
            total_vcpus: 120,
            ..crate::scaler::FleetDemand::default()
        };
        let scaler = RecScaler { rec: rec.clone() };
        let hysteresis = ScaleDownHysteresis::default();
        let pods = vec![
            ready_pod("a"),
            ready_pod("b"),
            ready_pod("replacement"),
            ready_pod("stuck"),
        ];

        let status = step(
            &autoscale_spec(),
            &scaler,
            &hysteresis,
            &NodeReadyTracker::default(),
            &rec,
            &rec,
            FleetObservation {
                pods: &pods,
                roll_idle: false,
                stuck_rolls: &["stuck".into()],
            },
        )
        .await
        .expect("autoscale step");

        assert!(status.blocks_roll, "must re-observe after named removal");
        let log = rec.log();
        let position = |prefix: &str| {
            log.iter()
                .position(|line| line.starts_with(prefix))
                .unwrap_or_else(|| panic!("missing {prefix:?} in {log:?}"))
        };
        assert!(position("drain ") < position("remove_node stuck"));
        assert!(position("remove_node stuck") < position("delete_host "));
        assert!(log.iter().any(|line| line == "roll_stuck stuck=None"));
    }

    #[tokio::test]
    async fn drive_one_happy_path_drains_removes_deletes() {
        let rec = Arc::new(Rec::default());
        let scaler = RecScaler { rec: rec.clone() };
        let act = actuator(&rec, &scaler, 1);
        // host_status reports 0 immediately → drains in one poll.
        let out = drive_victims(&act, &["node-a".to_string()]).await;
        assert_eq!(out, 0, "fully removed → nothing in flight");
        let log = rec.log();
        // cordon + annotate happen first, then drain → remove_node → delete_host.
        let pos = |needle: &str| {
            log.iter()
                .position(|l| l.starts_with(needle))
                .unwrap_or_else(|| panic!("missing {needle:?} in {log:?}"))
        };
        assert!(pos("cordon ") < pos("drain "), "cordon before drain");
        assert!(pos("drain ") < pos("remove_node "), "drain before remove");
        assert!(
            pos("remove_node ") < pos("delete_host "),
            "remove before delete"
        );
    }

    #[tokio::test]
    async fn drive_one_drain_timeout_releases_victim() {
        let rec = Arc::new(Rec::default());
        let scaler = RecScaler { rec: rec.clone() };
        // host_status always reports 2 running → never drains → times out.
        let host = HostId::from_node_name("node-b");
        rec.drain_progress
            .lock()
            .unwrap()
            .insert(host, std::iter::repeat_n(2u32, 100).collect());
        let act = actuator(&rec, &scaler, 1);
        let out = drive_victims(&act, &["node-b".to_string()]).await;
        assert_eq!(out, 0, "released → not counted as in flight");
        let log = rec.log();
        assert!(
            log.iter().any(|l| l.starts_with("uncordon ")),
            "a drain timeout uncordons the victim: {log:?}"
        );
        assert!(
            !log.iter().any(|l| l.starts_with("remove_node ")),
            "a timed-out victim is NEVER removed: {log:?}"
        );
    }

    /// ADR 0088: zero sandboxes is no longer sufficient — in-flight enable
    /// work (here a live capture job) holds the drain gate, and the wave
    /// releases the victim on timeout instead of removing the node under it.
    #[tokio::test(start_paused = true)]
    async fn drive_one_blocks_on_live_enable_work() {
        let rec = Arc::new(Rec::default());
        let scaler = RecScaler { rec: rec.clone() };
        let host = HostId::from_node_name("node-c");
        // 0 running sandboxes throughout (materialize boots no VM), but a
        // capture job stays live past the drain budget.
        rec.enable_work
            .lock()
            .unwrap()
            .insert(host, std::iter::repeat_n((0u32, 1u32), 100).collect());
        let act = actuator(&rec, &scaler, 1);
        let out = drive_victims(&act, &["node-c".to_string()]).await;
        assert_eq!(out, 0, "released → not counted as in flight");
        let log = rec.log();
        assert!(
            !log.iter().any(|l| l.starts_with("remove_node ")),
            "a node with live enable work is NEVER removed: {log:?}"
        );
        assert!(
            log.iter().any(|l| l.starts_with("uncordon ")),
            "the enable-work timeout releases the victim: {log:?}"
        );
    }

    #[test]
    fn assemble_joins_by_host_id_and_excludes_foreign_cordons() {
        fn pod(node: &str) -> PodInfo {
            PodInfo {
                name: format!("p-{node}"),
                node: node.into(),
                host_image: "i".into(),
                init_image: "n".into(),
                ready: true,
            }
        }
        fn load(node: &str, cordoned: bool) -> HostLoad {
            HostLoad {
                id: HostId::from_node_name(node),
                cordoned,
                running_sandboxes: 1,
                reserved_mib: 1_000,
                free_mib: 1_000,
                reserved_vcpus: 1,
                free_vcpus: 1,
            }
        }
        let pods = vec![pod("normal"), pod("foreign-cordon"), pod("our-victim")];
        let hosts = vec![
            load("normal", false),
            load("foreign-cordon", true), // cordoned, NOT annotated → excluded
            load("our-victim", true),     // cordoned + annotated → pinned
        ];
        let annotated = vec!["our-victim".to_string()];
        let assembled = assemble_wave_hosts(&pods, &hosts, &annotated);
        let nodes: Vec<&str> = assembled.iter().map(|h| h.node.as_str()).collect();
        assert!(nodes.contains(&"normal"));
        assert!(nodes.contains(&"our-victim"));
        assert!(
            !nodes.contains(&"foreign-cordon"),
            "a host cordoned by something other than this wave is off-limits"
        );
        assert!(
            assembled
                .iter()
                .find(|h| h.node == "our-victim")
                .unwrap()
                .pinned
        );
        assert!(
            !assembled
                .iter()
                .find(|h| h.node == "normal")
                .unwrap()
                .pinned
        );
    }

    #[tokio::test]
    async fn abort_wave_uncordons_and_deannotates() {
        let rec = Arc::new(Rec::default());
        let scaler = RecScaler { rec: rec.clone() };
        let act = actuator(&rec, &scaler, 1);
        abort_wave(&act, &["node-x".to_string(), "node-y".to_string()]).await;
        let log = rec.log();
        assert!(log.iter().filter(|l| l.starts_with("uncordon ")).count() == 2);
        assert!(log.iter().any(|l| l == "victim node-x=None"));
        assert!(log.iter().any(|l| l == "victim node-y=None"));
        assert!(
            !log.iter().any(|l| l.starts_with("remove_node ")),
            "abort never removes a node"
        );
    }
}
