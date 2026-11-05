//! ADR 0126: the drain before an incompatible upgrade, and the roll after it.
//!
//! The drain runs as a Helm `pre-upgrade` hook against the coordinator that
//! still serves, so it uses only RPCs the previous release has. `plan` is the
//! pure step; `drain` repeats it until no host the target build cannot adopt
//! runs a session VM.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use k8s_openapi::api::apps::v1::DaemonSet;
use k8s_openapi::api::core::v1::{ConfigMap, Pod};
use kube::api::{Api, DeleteParams, ListParams, PostParams};

use crate::coord::{CoordClient, SessionOps, UpgradeHost, UpgradeSession};
use crate::error::OperatorError;

/// The drain's cordon owner. The coordinator lifts it when the host
/// registers with a higher reattach generation.
const DRAIN_CORDON_OWNER: &str = "admin";

/// How many session actions run at once.
const CONCURRENCY: usize = 4;

#[derive(Debug, PartialEq, Eq)]
pub enum DrainAction {
    Cordon(engram_core::HostId),
    /// Checkpoint the session if it has none, then evict it.
    Evict(String),
    /// A parked session's VM is paused in place; resume it so it can be
    /// evicted.
    Resume(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum DrainStep {
    /// Every live host already runs the target's generation.
    NotNeeded,
    /// No session VM is left on a host the target cannot adopt. The
    /// unreachable sessions keep their last checkpoint and resume from it.
    Done { unreachable: Vec<String> },
    Work {
        actions: Vec<DrainAction>,
        /// Sessions in a transition that ends on its own.
        waiting: Vec<String>,
    },
}

/// One drain step over the coordinator's view of the fleet.
pub fn plan(target: u32, hosts: &[UpgradeHost], sessions: &[UpgradeSession]) -> DrainStep {
    let old: BTreeSet<_> = hosts
        .iter()
        .filter(|h| h.live && h.reattach_generation < target)
        .map(|h| h.id)
        .collect();
    if old.is_empty() {
        return DrainStep::NotNeeded;
    }
    let mut actions: Vec<DrainAction> = hosts
        .iter()
        .filter(|h| old.contains(&h.id) && !h.cordoned)
        .map(|h| DrainAction::Cordon(h.id))
        .collect();
    let (mut waiting, mut unreachable) = (Vec::new(), Vec::new());
    for s in sessions {
        if !s.host_id.is_some_and(|h| old.contains(&h)) {
            continue;
        }
        match s.state.as_str() {
            "active" => actions.push(DrainAction::Evict(s.id.clone())),
            "parked" => actions.push(DrainAction::Resume(s.id.clone())),
            "pending" | "created" | "evacuating" | "evicting" => waiting.push(s.id.clone()),
            "unreachable" => unreachable.push(s.id.clone()),
            _ => {}
        }
    }
    if actions.is_empty() && waiting.is_empty() {
        DrainStep::Done { unreachable }
    } else {
        DrainStep::Work { actions, waiting }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum DrainOutcome {
    NotNeeded,
    Drained { unreachable: Vec<String> },
}

/// Repeat [`plan`] until it is done, or fail at `deadline` with the sessions
/// still left and the last error of each.
pub async fn drain(
    fleet: &CoordClient,
    sessions: &SessionOps,
    target: u32,
    deadline: Duration,
    poll: Duration,
) -> Result<DrainOutcome, OperatorError> {
    let started = crate::time_source::metrics_now_tokio();
    let mut last_error: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let hosts = fleet.list_upgrade_hosts().await?;
        let listed = sessions.list().await?;
        let (actions, waiting) = match plan(target, &hosts, &listed) {
            DrainStep::NotNeeded => return Ok(DrainOutcome::NotNeeded),
            DrainStep::Done { unreachable } => {
                return Ok(DrainOutcome::Drained { unreachable });
            }
            DrainStep::Work { actions, waiting } => (actions, waiting),
        };
        tracing::info!(
            actions = actions.len(),
            waiting = waiting.len(),
            "upgrade drain step"
        );
        for chunk in actions.chunks(CONCURRENCY) {
            let results =
                futures::future::join_all(chunk.iter().map(|a| run(fleet, sessions, a))).await;
            for (action, result) in chunk.iter().zip(results) {
                let key = format!("{action:?}");
                match result {
                    Ok(()) => {
                        last_error.remove(&key);
                    }
                    Err(e) => {
                        tracing::warn!(?action, error = %e, "upgrade drain action failed; retrying");
                        last_error.insert(key, e.to_string());
                    }
                }
            }
        }
        if started.elapsed() >= deadline {
            let left: Vec<String> = actions
                .iter()
                .map(|a| format!("{a:?}"))
                .chain(waiting.iter().map(|s| format!("waiting {s}")))
                .collect();
            return Err(OperatorError::Upgrade(format!(
                "the drain did not finish in {}s; left: {}; last errors: {:?}",
                deadline.as_secs(),
                left.join(", "),
                last_error
            )));
        }
        tokio::time::sleep(poll).await;
    }
}

async fn run(
    fleet: &CoordClient,
    sessions: &SessionOps,
    action: &DrainAction,
) -> Result<(), OperatorError> {
    match action {
        DrainAction::Cordon(host) => fleet.cordon(*host, DRAIN_CORDON_OWNER).await,
        DrainAction::Resume(id) => sessions.resume(id).await,
        DrainAction::Evict(id) => {
            if !sessions.has_checkpoint(id).await? {
                sessions.snapshot(id).await?;
            }
            sessions.evict(id).await
        }
    }
}

/// Record, in `namespace`, that a drain ran, so the post-upgrade roll
/// knows the nodes are empty.
pub async fn write_marker(
    client: kube::Client,
    namespace: &str,
    name: &str,
) -> Result<(), OperatorError> {
    let api: Api<ConfigMap> = Api::namespaced(client, namespace);
    let marker = ConfigMap {
        metadata: kube::api::ObjectMeta {
            name: Some(name.into()),
            ..Default::default()
        },
        data: Some(BTreeMap::from([(
            "reattach_generation".into(),
            engram_protocol::REATTACH_GENERATION.to_string(),
        )])),
        ..Default::default()
    };
    match api.create(&PostParams::default(), &marker).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// The DaemonSet pods not on its current template.
pub fn stale_pods(pods: &[Pod], ds_generation: i64) -> Vec<String> {
    let current = ds_generation.to_string();
    pods.iter()
        .filter(|p| {
            p.metadata
                .labels
                .as_ref()
                .and_then(|l| l.get("pod-template-generation"))
                != Some(&current)
        })
        .filter_map(|p| p.metadata.name.clone())
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub enum RollOutcome {
    /// No drain ran, so this upgrade rolls the usual way.
    NoDrain,
    Rolled {
        deleted: Vec<String>,
    },
}

/// After a drain: delete the host-agent pods not on the DaemonSet's current
/// template, so the `OnDelete` DaemonSet rolls onto the new build, then
/// remove the marker. With the operator enabled the operator rolls them.
pub async fn roll(
    client: kube::Client,
    fleet: &CoordClient,
    namespace: &str,
    daemon_set: &str,
    marker: &str,
    operator_rolls: bool,
) -> Result<RollOutcome, OperatorError> {
    let markers: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    if markers.get_opt(marker).await?.is_none() {
        return Ok(RollOutcome::NoDrain);
    }
    let busy: Vec<_> = fleet
        .list_upgrade_hosts()
        .await?
        .into_iter()
        .filter(|h| {
            h.live
                && h.reattach_generation < engram_protocol::REATTACH_GENERATION
                && h.running_sandboxes > 0
        })
        .map(|h| h.id.to_string())
        .collect();
    if !busy.is_empty() {
        return Err(OperatorError::Upgrade(format!(
            "hosts the new build cannot adopt still run sandboxes: {}",
            busy.join(", ")
        )));
    }
    let mut deleted = Vec::new();
    if !operator_rolls {
        let ds = Api::<DaemonSet>::namespaced(client.clone(), namespace)
            .get(daemon_set)
            .await?;
        let generation = ds.metadata.generation.ok_or_else(|| {
            OperatorError::Invalid(format!("DaemonSet {daemon_set} has no generation"))
        })?;
        let selector = ds
            .spec
            .selector
            .match_labels
            .as_ref()
            .map(|labels| {
                labels
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .ok_or_else(|| {
                OperatorError::Invalid(format!("DaemonSet {daemon_set} has no label selector"))
            })?;
        let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
        let listed = pods.list(&ListParams::default().labels(&selector)).await?;
        for name in stale_pods(&listed.items, generation) {
            pods.delete(&name, &DeleteParams::default()).await?;
            deleted.push(name);
        }
    }
    markers.delete(marker, &DeleteParams::default()).await?;
    Ok(RollOutcome::Rolled { deleted })
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::HostId;

    fn host(generation: u32, cordoned: bool) -> UpgradeHost {
        UpgradeHost {
            id: HostId::new(),
            live: true,
            cordoned,
            running_sandboxes: 0,
            reattach_generation: generation,
        }
    }

    fn session(id: &str, state: &str, host: &UpgradeHost) -> UpgradeSession {
        UpgradeSession {
            id: id.into(),
            state: state.into(),
            host_id: Some(host.id),
        }
    }

    #[test]
    fn a_fleet_on_the_target_generation_needs_no_drain() {
        let h = host(1, false);
        let sessions = [session("s1", "active", &h)];
        assert_eq!(plan(1, &[h], &sessions), DrainStep::NotNeeded);
    }

    #[test]
    fn dead_hosts_do_not_need_a_drain() {
        let mut h = host(0, false);
        h.live = false;
        assert_eq!(plan(1, &[h], &[]), DrainStep::NotNeeded);
    }

    #[test]
    fn an_old_host_is_cordoned_and_each_session_state_gets_its_action() {
        let old = host(0, false);
        let new = host(1, false);
        let sessions = [
            session("active", "active", &old),
            session("parked", "parked", &old),
            session("evicting", "evicting", &old),
            session("pending", "pending", &old),
            session("unreachable", "unreachable", &old),
            session("idle", "idle", &old),
            session("on-new-host", "active", &new),
        ];
        assert_eq!(
            plan(1, &[old.clone(), new], &sessions),
            DrainStep::Work {
                actions: vec![
                    DrainAction::Cordon(old.id),
                    DrainAction::Evict("active".into()),
                    DrainAction::Resume("parked".into()),
                ],
                waiting: vec!["evicting".into(), "pending".into()],
            }
        );
    }

    #[test]
    fn a_cordoned_empty_old_host_is_done_and_names_unreachable_sessions() {
        let old = host(0, true);
        let sessions = [
            session("gone", "idle", &old),
            session("stuck", "unreachable", &old),
        ];
        assert_eq!(
            plan(1, &[old], &sessions),
            DrainStep::Done {
                unreachable: vec!["stuck".into()]
            }
        );
    }

    #[test]
    fn stale_pods_are_the_ones_off_the_current_template() {
        let pod = |name: &str, generation: Option<&str>| Pod {
            metadata: kube::api::ObjectMeta {
                name: Some(name.into()),
                labels: generation.map(|g| {
                    BTreeMap::from([("pod-template-generation".to_string(), g.to_string())])
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let pods = [
            pod("old", Some("3")),
            pod("new", Some("4")),
            pod("unlabelled", None),
        ];
        assert_eq!(stale_pods(&pods, 4), vec!["old", "unlabelled"]);
    }
}
