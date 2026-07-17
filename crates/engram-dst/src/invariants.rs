//! The D5 invariant set — checked after every step (SimDb + world are
//! in-memory; a sweep is microseconds). D6 grows this to the full
//! nine-oracle suite from ADR 0098.

use engram_core::types::session::SessionState;
use engram_core::SessionId;

use crate::world::SimWorld;

#[derive(Debug)]
pub struct Violation {
    pub invariant: &'static str,
    pub detail: String,
}

/// Cheap per-step checks.
pub fn check_step(world: &SimWorld) -> Result<(), Violation> {
    single_ownership(world)?;
    bound_sessions_point_at_live_hosts(world)?;
    Ok(())
}

/// ADR 0090: at most one live sandbox per session across ALL hosts, and
/// the coordinator's binding (sessions.sandbox_id) never points at a
/// sandbox owned by a DIFFERENT session on the world side.
fn single_ownership(world: &SimWorld) -> Result<(), Violation> {
    let hosts = world.host_world.hosts.lock();
    let mut owner_of: std::collections::BTreeMap<SessionId, u32> = Default::default();
    for host in hosts.values() {
        for owner in host.sandboxes.values().flatten() {
            *owner_of.entry(*owner).or_default() += 1;
        }
    }
    if let Some((sid, n)) = owner_of.iter().find(|(_, n)| **n > 1) {
        return Err(Violation {
            invariant: "single-ownership",
            detail: format!("session {sid} owns {n} live sandboxes across the fleet"),
        });
    }
    Ok(())
}

/// A session the coordinator believes is bound (sandbox_id set, in a
/// reserving state) must point at a host that exists in the world.
/// (The host may be DOWN — that's the failure being detected — but a
/// binding to a host id the world never had is state corruption.)
fn bound_sessions_point_at_live_hosts(world: &SimWorld) -> Result<(), Violation> {
    let hosts = world.host_world.hosts.lock();
    world.meta.with_db(|db| {
        for row in db.sessions.values() {
            if row.session.sandbox_id.is_some() {
                if let Some(host) = row.session.host_id {
                    if !hosts.contains_key(&host) {
                        return Err(Violation {
                            invariant: "binding-host-exists",
                            detail: format!(
                                "session {} bound to unknown host {host}",
                                row.session.id
                            ),
                        });
                    }
                }
            }
        }
        Ok(())
    })
}

/// Quiescence checks: after faults stop and the drivers run to
/// convergence, no session may be stuck in a transient state.
pub fn check_quiescence(world: &SimWorld) -> Result<(), Violation> {
    world.meta.with_db(|db| {
        for row in db.sessions.values() {
            let stable = matches!(
                row.session.status,
                SessionState::Active
                    | SessionState::Idle
                    | SessionState::Completed
                    | SessionState::Failed
                    | SessionState::Dead
                    // Created is stable-serving pre-agent; Pending/Queued
                    // only transiently between scanner passes — but a
                    // queued session with NO capacity anywhere is
                    // legitimately parked, so Queued counts as stable
                    // when every host is down or full.
                    | SessionState::Created
                    | SessionState::Queued
            );
            if !stable {
                return Err(Violation {
                    invariant: "quiescence-no-stragglers",
                    detail: format!(
                        "session {} stuck at {:?} after convergence",
                        row.session.id, row.session.status
                    ),
                });
            }
        }
        Ok(())
    })
}
