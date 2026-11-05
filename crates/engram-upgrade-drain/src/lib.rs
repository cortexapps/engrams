//! The drain before an upgrade whose host agents cannot adopt the VMs that
//! run now (ADR 0126). It cordons every live host on an older reattach
//! generation and evicts each session there to a checkpoint. Both charts run
//! it as a `pre-upgrade` hook: the coordinator image runs
//! `engram-coordinator upgrade-drain`, the host-operator image runs
//! `engram-host-operator upgrade-drain`.
//!
//! `plan` is the pure step; `drain` repeats it until no host the target
//! build cannot adopt runs a session VM.

mod client;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

pub use client::{channel, parse_host_id, Bearer, Coordinator, UpgradeHost, UpgradeSession};

/// The drain's cordon owner. The coordinator lifts it when the host
/// registers with a higher reattach generation.
const DRAIN_CORDON_OWNER: &str = "admin";

/// How many session actions run at once.
const CONCURRENCY: usize = 4;

const POLL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum DrainError {
    Rpc {
        op: &'static str,
        // Boxed: `tonic::Status` is large (clippy::result_large_err).
        status: Box<tonic::Status>,
    },
    Invalid(String),
    /// The drain did not finish in its budget.
    Timeout(String),
}

impl DrainError {
    fn rpc(op: &'static str, status: tonic::Status) -> Self {
        Self::Rpc {
            op,
            status: Box::new(status),
        }
    }
}

impl fmt::Display for DrainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc { op, status } => write!(f, "coordinator {op} failed: {status}"),
            Self::Invalid(msg) => write!(f, "upgrade drain: {msg}"),
            Self::Timeout(msg) => write!(f, "upgrade drain: {msg}"),
        }
    }
}

impl std::error::Error for DrainError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rpc { status, .. } => Some(status.as_ref()),
            _ => None,
        }
    }
}

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
    coord: &Coordinator,
    target: u32,
    deadline: Duration,
    poll: Duration,
) -> Result<DrainOutcome, DrainError> {
    let started = monotonic_now();
    let mut last_error: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let hosts = coord.list_hosts().await?;
        let sessions = coord.list_sessions().await?;
        let (actions, waiting) = match plan(target, &hosts, &sessions) {
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
            let results = futures::future::join_all(chunk.iter().map(|a| run(coord, a))).await;
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
            return Err(DrainError::Timeout(format!(
                "the drain did not finish in {}s; left: {}; last errors: {:?}",
                deadline.as_secs(),
                left.join(", "),
                last_error
            )));
        }
        tokio::time::sleep(poll).await;
    }
}

async fn run(coord: &Coordinator, action: &DrainAction) -> Result<(), DrainError> {
    match action {
        DrainAction::Cordon(host) => coord.cordon(*host, DRAIN_CORDON_OWNER).await,
        DrainAction::Resume(id) => coord.resume(id).await,
        DrainAction::Evict(id) => {
            if !coord.has_checkpoint(id).await? {
                coord.snapshot(id).await?;
            }
            coord.evict(id).await
        }
    }
}

/// The coordinator's app-gRPC address and one bearer token, from
/// `ENGRAM_COORDINATOR_URL` and `ENGRAM_COORDINATOR_TOKEN`. The token
/// variable may hold the coordinator's comma-separated allowlist; any entry
/// is accepted.
pub fn coordinator_from_env() -> Result<(String, Option<String>), DrainError> {
    let url = std::env::var("ENGRAM_COORDINATOR_URL")
        .map_err(|_| DrainError::Invalid("ENGRAM_COORDINATOR_URL must be set".into()))?;
    let token = std::env::var("ENGRAM_COORDINATOR_TOKEN")
        .ok()
        .and_then(|t| {
            t.split(',')
                .map(str::trim)
                .find(|t| !t.is_empty())
                .map(String::from)
        });
    Ok((url, token))
}

/// The hook: drain toward this build's reattach generation, with the
/// budget in `ENGRAM_UPGRADE_TIMEOUT_SECS` (default an hour).
pub async fn run_from_env() -> Result<DrainOutcome, DrainError> {
    let (url, token) = coordinator_from_env()?;
    let budget = Duration::from_secs(match std::env::var("ENGRAM_UPGRADE_TIMEOUT_SECS") {
        Ok(v) => v
            .parse()
            .map_err(|e| DrainError::Invalid(format!("ENGRAM_UPGRADE_TIMEOUT_SECS: {e}")))?,
        Err(_) => 3600,
    });
    let target = engram_protocol::REATTACH_GENERATION;
    let outcome = drain(&Coordinator::new(&url, token, budget), target, budget, POLL).await?;
    match &outcome {
        DrainOutcome::NotNeeded => tracing::info!(
            target,
            "every live host runs the target's reattach generation; no drain"
        ),
        DrainOutcome::Drained { unreachable } => tracing::info!(
            target,
            ?unreachable,
            "drained every host the target cannot adopt; unreachable sessions resume from their last checkpoint",
        ),
    }
    Ok(outcome)
}

/// The drain budget is real elapsed time; the drain has no simulation clock
/// seam (ADR 0098 P1 carve-out).
#[allow(clippy::disallowed_methods)] // unsimulated upgrade hook deadline (ADR 0098 P1)
fn monotonic_now() -> tokio::time::Instant {
    tokio::time::Instant::now()
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
}
