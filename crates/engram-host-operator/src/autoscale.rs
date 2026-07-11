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
//! The actuation rides two seams — [`CoordApi`] (coordinator admin calls) and
//! [`NodeOps`] (K8s node cordon + annotation) — so the executor is exercised
//! against recording mocks without a cluster or a coordinator.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
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
}

/// Live K8s implementation of [`NodeOps`].
pub struct K8sNodeOps {
    pub client: Client,
}

#[async_trait]
impl NodeOps for K8sNodeOps {
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
        use kube::api::ListParams;
        use kube::ResourceExt;
        let nodes: Api<Node> = Api::all(self.client.clone());
        let list = nodes.list(&ListParams::default()).await?;
        Ok(list
            .into_iter()
            .filter(|n| {
                n.annotations()
                    .get(VICTIM_ANNOTATION)
                    .map(|v| v == fleet)
                    .unwrap_or(false)
            })
            .map(|n| n.name_any())
            .collect())
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
    let scale_up = i.desired > i.physical;
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
    /// A wave is still in flight (victims cordoned but not yet removed) — the
    /// reconcile loop must NOT start an image roll and should requeue soon.
    pub wave_in_flight: bool,
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
            Ok(()) => tracing::info!(%node, %host, "wave abort: uncordoned + de-annotated victim"),
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
    tracing::info!(%node, %host, "wave: victim drained + removed + deregistered");
    DriveOutcome::Removed
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
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match coord.host_status(host).await? {
            None => return Ok(()), // already deregistered
            Some(st) if st.running_sandboxes == 0 && !st.has_enable_work() => return Ok(()),
            Some(st) => {
                if tokio::time::Instant::now() >= deadline {
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
    scaledown_ticks: &std::sync::atomic::AtomicU32,
    coord: &dyn CoordApi,
    nodes: &dyn NodeOps,
    pods: &[PodInfo],
    roll_idle: bool,
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
    let physical = pods.len() as u32;
    // The annotation value keys wave-victims to THIS node pool (distinguishing
    // them from a manual cordon, and from another fleet's wave).
    let fleet_key = a.node_pool.as_str();
    let annotated = nodes.annotated_victims(fleet_key).await?;
    let wave_in_flight = !annotated.is_empty();

    // Hysteresis: accumulate only while a fresh scale-down target persists.
    let scale_down_target = a.scale_down.enabled()
        && desired < current
        && demand.queued_sessions == 0
        && desired <= physical;
    let hysteresis_ready = if scale_down_target && !wave_in_flight {
        let ticks = scaledown_ticks.fetch_add(1, Ordering::Relaxed) + 1;
        ticks >= a.scale_down_hysteresis_ticks.max(1)
    } else {
        scaledown_ticks.store(0, Ordering::Relaxed);
        false
    };

    let action = plan_step(StepInputs {
        desired,
        current,
        physical,
        queued_sessions: demand.queued_sessions,
        scale_down_enabled: a.scale_down.enabled(),
        hysteresis_ready,
        roll_idle,
        wave_in_flight,
    });
    tracing::info!(
        node_pool = %a.node_pool, current, physical, desired,
        queued = demand.queued_sessions, wave_in_flight, ?action,
        "autoscale step"
    );

    let act = WaveActuator {
        coord,
        nodes,
        scaler,
        fleet: fleet_key,
        node_pool: &a.node_pool,
        max_concurrent_drains: a.max_concurrent_drains.max(1),
        drain_timeout: Duration::from_secs(spec.drain_timeout_seconds),
    };

    match action {
        StepAction::AbortAndGrow => {
            if wave_in_flight {
                abort_wave(&act, &annotated).await;
            }
            if desired > physical {
                scaler.set_size(&a.node_pool, desired).await?;
                tracing::info!(node_pool=%a.node_pool, desired, physical, "autoscale: grew pool");
            }
            Ok(AutoscaleStatus::default())
        }
        StepAction::AbortOnly => {
            if wave_in_flight {
                abort_wave(&act, &annotated).await;
            }
            Ok(AutoscaleStatus::default())
        }
        StepAction::Hold => Ok(AutoscaleStatus::default()),
        StepAction::StartWave | StepAction::ContinueWave => {
            let hosts = coord.list_hosts().await?;
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
                wave_in_flight: in_flight > 0,
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
    fn queue_pressure_aborts_even_mid_wave() {
        let i = StepInputs {
            queued_sessions: 3,
            wave_in_flight: true,
            desired: 5,
            physical: 5,
            ..inputs()
        };
        assert_eq!(plan_step(i), StepAction::AbortOnly);
    }

    #[test]
    fn scale_up_aborts_and_grows() {
        let i = StepInputs {
            desired: 8,
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

    // ---- Recording-mock actuation tests ----

    #[derive(Default)]
    struct Rec {
        log: Mutex<Vec<String>>,
        /// host_id → running_sandboxes returned by host_status (one entry per
        /// call, popped front; empty → 0).
        drain_progress: Mutex<HashMap<HostId, std::collections::VecDeque<u32>>>,
        /// ADR 0088: host_id → (live_materializes, live_capture_jobs) per
        /// host_status call (popped front; empty → (0, 0)).
        enable_work: Mutex<HashMap<HostId, std::collections::VecDeque<(u32, u32)>>>,
        annotated: Mutex<Vec<String>>,
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
            Ok(crate::scaler::FleetDemand::default())
        }
        async fn list_hosts(&self) -> Result<Vec<HostLoad>, OperatorError> {
            Ok(vec![])
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
