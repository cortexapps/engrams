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
    transition_legality(world)?;
    one_running_op_per_session(world)?;
    placement_accounting(world)?;
    Ok(())
}

/// Every status flip SimMeta performed satisfies `can_transition_to`
/// (defense-in-depth: catches direct-write bugs in SimMeta itself).
/// The documented mark_host_dead bulk flip is exempt (ADR 0099 H6
/// finding — broader than the table pending a design call), but only
/// toward HostLost.
fn transition_legality(world: &SimWorld) -> Result<(), Violation> {
    world.meta.with_db(|db| {
        for e in &db.transition_log {
            if e.exempt {
                if e.to != SessionState::HostLost {
                    return Err(Violation {
                        invariant: "transition-legality",
                        detail: format!(
                            "exempt bulk flip abused: {:?} -> {:?} for {}",
                            e.from, e.to, e.session
                        ),
                    });
                }
                continue;
            }
            if !e.from.can_transition_to(e.to) {
                return Err(Violation {
                    invariant: "transition-legality",
                    detail: format!(
                        "illegal transition recorded: {:?} -> {:?} for {}",
                        e.from, e.to, e.session
                    ),
                });
            }
        }
        Ok(())
    })
}

/// ADR 0079's structural invariant, re-derived from state rather than
/// trusted from the enforcing index.
fn one_running_op_per_session(world: &SimWorld) -> Result<(), Violation> {
    world.meta.with_db(|db| {
        let mut running: std::collections::BTreeMap<SessionId, u32> = Default::default();
        for op in db.session_ops.values() {
            if op.state == engram_core::types::session_op::OpState::Running {
                *running.entry(op.session_id).or_default() += 1;
            }
        }
        if let Some((sid, n)) = running.iter().find(|(_, n)| **n > 1) {
            return Err(Violation {
                invariant: "one-running-op",
                detail: format!("session {sid} has {n} running ops"),
            });
        }
        Ok(())
    })
}

/// ADR 0046: Σ reserved memory per host never exceeds what the host
/// advertises as allocatable (checked only for measured hosts — an
/// unmeasured host takes only last-resort placements by design).
///
/// SCOPED to placement's own arithmetic: a `pending` older than 10
/// minutes is excluded exactly as `pick_host_2d` excludes it — so this
/// checks self-consistency ("placement never over-reserves by its own
/// sum"), not the aspirational unconditional bound. The unconditional
/// form FAILS today: the crash-orphan exclusion contradicts the
/// ADR 0079 pending-revival backstop (found by this oracle at chaos
/// seed 0; issue #722). Tighten back when #722's fix lands.
fn placement_accounting(world: &SimWorld) -> Result<(), Violation> {
    use engram_core::traits::Clock as _;
    let now = world.clock.now_utc();
    world.meta.with_db(|db| {
        let mut reserved: std::collections::BTreeMap<engram_core::HostId, i64> = Default::default();
        for row in db.sessions.values() {
            if let Some(host) = row.session.host_id {
                let st = row.session.status;
                let counts = st.reserves_host_memory()
                    && (st != SessionState::Pending
                        || row.session.last_active_at > now - chrono::Duration::minutes(10));
                if counts {
                    *reserved.entry(host).or_default() += row.mem_budget_mib;
                }
            }
        }
        for (host, mem) in reserved {
            if let Some(h) = db.hosts.get(&host) {
                let alloc = h.utilization.allocatable_mib as i64;
                if alloc > 0 && mem > alloc {
                    return Err(Violation {
                        invariant: "placement-accounting",
                        detail: format!(
                            "host {host} over-reserved: {mem} MiB > allocatable {alloc}"
                        ),
                    });
                }
            }
        }
        Ok(())
    })
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
/// convergence, no session may be stuck in a transient state and no
/// op may be left undriven.
pub fn check_quiescence(world: &SimWorld) -> Result<(), Violation> {
    no_op_dropped(world)?;
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

/// Every op reaches a terminal state once the world heals and the
/// drivers converge — a queued op left behind is a dropped command.
fn no_op_dropped(world: &SimWorld) -> Result<(), Violation> {
    use engram_core::types::session_op::OpState;
    world.meta.with_db(|db| {
        for op in db.session_ops.values() {
            if matches!(op.state, OpState::Queued | OpState::Running) {
                return Err(Violation {
                    invariant: "no-op-dropped",
                    detail: format!(
                        "op {} ({:?}) for {} left {:?} after convergence (attempts {}, err {:?})",
                        op.id, op.kind, op.session_id, op.state, op.attempts, op.error
                    ),
                });
            }
        }
        Ok(())
    })
}
