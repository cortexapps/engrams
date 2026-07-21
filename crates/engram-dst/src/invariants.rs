//! The D5 invariant set — checked after every step (SimDb + world are
//! in-memory; a sweep is microseconds). D6 grows this to the full
//! nine-oracle suite from ADR 0098.

use std::collections::{BTreeMap, BTreeSet};

use engram_core::types::session::{QueueOrigin, SessionState};
use engram_core::SessionId;

use crate::world::SimWorld;

#[derive(Debug)]
pub struct Violation {
    pub invariant: &'static str,
    pub detail: String,
}

#[derive(Default)]
pub struct Oracles {
    epochs: BTreeMap<SessionId, (i64, i64, i64)>,
}

impl Oracles {
    /// Cheap per-step checks, including stateful outcome checks whose
    /// observations span scheduler steps.
    pub fn check_step(&mut self, world: &SimWorld) -> Result<(), Violation> {
        single_ownership(world)?;
        bound_sessions_point_at_live_hosts(world)?;
        transition_legality(world)?;
        one_running_op_per_session(world)?;
        placement_accounting(world)?;
        self.epoch_monotonicity(world)?;
        sandbox_owners_agree(world)?;
        snapshot_safety(world)?;
        Ok(())
    }

    /// ADR 0098: epochs observed in committed session rows never regress.
    /// SimMeta rejects stale-epoch writes internally; this checks the
    /// outcome against rebuilt rows and direct-write corruption too.
    fn epoch_monotonicity(&mut self, world: &SimWorld) -> Result<(), Violation> {
        world.meta.with_db(|db| {
            let mut present = BTreeSet::new();
            for (sid, row) in &db.sessions {
                present.insert(*sid);
                let observed = (row.current_epoch, row.binding_epoch, row.recovery_epoch);
                if let Some(previous) = self.epochs.get(sid) {
                    if observed.0 < previous.0 || observed.1 < previous.1 || observed.2 < previous.2
                    {
                        return Err(Violation {
                            invariant: "epoch-monotonic",
                            detail: format!(
                                "session {sid} epochs regressed from {previous:?} to {observed:?}"
                            ),
                        });
                    }
                }
                self.epochs.insert(*sid, observed);
            }
            self.epochs.retain(|sid, _| present.contains(sid));
            Ok(())
        })
    }
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
/// UNCONDITIONAL bound (the ADR's original intent): EVERY session that
/// holds a host reservation (any `reserves_host_memory` state pinned to a
/// host) is summed — no crash-orphan exclusion, because a Pending pinned
/// to a host physically holds that slot until it leaves the reserving
/// state, and the ADR 0079 pending-revival backstop can put it back on the
/// CPU. History: #722's crash-orphan exclusion (placement stopped counting
/// an aged Pending) contradicted that backstop, so a revival could land on
/// re-sold capacity (Σ reserved > allocatable). #775 scoped THIS oracle to
/// self-consistency (sharing `pending_counts` with `pick_host_2d`) as a
/// stopgap *precisely because the product was broken* — that scoping could
/// only ever prove placement agreed with itself, never that the physical
/// sum was safe. With the R3 fix landed (reservation-counting no longer
/// keys on wall-age; reclamation is a real Pending→terminal transition, not
/// a placement-side exclusion — one authority), the unconditional bound
/// holds and is restored here as the real oracle.
fn placement_accounting(world: &SimWorld) -> Result<(), Violation> {
    world.meta.with_db(|db| {
        let mut reserved: std::collections::BTreeMap<engram_core::HostId, i64> = Default::default();
        for row in db.sessions.values() {
            if let Some(host) = row.session.host_id {
                let st = row.session.status;
                let counts = st.reserves_host_memory();
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

/// ADR 0090: a coordinator sandbox binding never names a world sandbox
/// that is owned by a different session. Down-host world memory remains
/// authoritative for detecting this corruption; an unknown owner is fine.
fn sandbox_owners_agree(world: &SimWorld) -> Result<(), Violation> {
    let hosts = world.host_world.hosts.lock();
    world.meta.with_db(|db| {
        for row in db.sessions.values() {
            let Some(sandbox) = row.session.sandbox_id else {
                continue;
            };
            for (host_id, host) in hosts.iter() {
                if let Some(Some(owner)) = host.sandboxes.get(&sandbox) {
                    if *owner != row.session.id {
                        return Err(Violation {
                            invariant: "sandbox-ownership-agreement",
                            detail: format!(
                                "session {} points at sandbox {sandbox} on host {host_id}, owned by {owner}",
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

/// ADR 0098 durable-state safety: states with no live VM retain a
/// recoverable snapshot row or a live-disk manifest pointer. The sim
/// models recoverability at ROW granularity; chunk/blob-tier durability
/// of manifest contents is host-sim territory and is not asserted here.
fn snapshot_safety(world: &SimWorld) -> Result<(), Violation> {
    world.meta.with_db(|db| {
        for row in db.sessions.values() {
            let requires_durable = matches!(
                row.session.status,
                SessionState::Idle | SessionState::Evacuating
            ) || (row.session.status == SessionState::Queued
                && row.queue_origin == Some(QueueOrigin::Resume));
            if !requires_durable {
                continue;
            }
            let has_snapshot = db.snapshots.values().any(|snapshot| {
                snapshot.session_id == Some(row.session.id) && snapshot.recoverable
            });
            if !has_snapshot && row.session.live_disk_manifest.is_none() {
                return Err(Violation {
                    invariant: "snapshot-safety",
                    detail: format!(
                        "session {} at {:?} has no recoverable durable copy",
                        row.session.id, row.session.status
                    ),
                });
            }
        }
        Ok(())
    })
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
    no_orphan_sandboxes(world)?;
    let sessions = world.meta.with_db(|db| {
        db.sessions
            .values()
            .map(|row| {
                (
                    row.session.id,
                    row.session.status,
                    row.mem_budget_mib,
                    i64::from(row.cpu_budget_vcpus),
                )
            })
            .collect::<Vec<_>>()
    });
    for (sid, status, mem_budget_mib, cpu_budget_vcpus) in sessions {
        if status == SessionState::Queued {
            if let Some(host) = world
                .meta
                .oracle_pick_any_host(mem_budget_mib, cpu_budget_vcpus)
            {
                return Err(Violation {
                    invariant: "quiescence-queued-with-capacity",
                    detail: format!("session {sid} remains queued with capacity on host {host}"),
                });
            }
            continue;
        }
        let stable = matches!(
            status,
            SessionState::Active
                // ADR 0101 C: a parked VM at rest is the ladder working
                // as designed (host has headroom; the reaper holds the
                // park until pressure or the hard TTL). `Evicting` stays
                // UNSTABLE on purpose: post-ADR-0101-C it means a
                // descent's settle never landed — exactly a violation.
                | SessionState::Parked
                | SessionState::Idle
                | SessionState::Completed
                | SessionState::Failed
                | SessionState::Dead
                | SessionState::Created
        );
        if !stable {
            return Err(Violation {
                invariant: "quiescence-no-stragglers",
                detail: format!("session {} stuck at {:?} after convergence", sid, status),
            });
        }
    }
    Ok(())
}

/// ADR 0101 C oracle (the livelock-class pin): the highest `session_ops`
/// row id in the world — the op-mint high-water mark. The scheduler
/// snapshots it at quiescence, runs further full driver rounds, and
/// asserts it does not move: **a settled world mints no ops.** This is
/// the property status-based quiescence cannot see — the ADR 0077×0090
/// incident ran 2.5 days with every op row TERMINAL and every status
/// frozen while the scanner minted a fresh enqueue→skip op each tick.
pub fn op_mint_high_water(world: &SimWorld) -> i64 {
    world.meta.with_db(|db| {
        db.session_ops
            .values()
            .map(|op| op.id)
            .max()
            .unwrap_or_default()
    })
}

/// The op-mint check paired with [`op_mint_high_water`]: fails if any op
/// row was minted past the recorded high-water mark.
pub fn check_no_ops_minted_since(world: &SimWorld, high_water: i64) -> Result<(), Violation> {
    let offenders = world.meta.with_db(|db| {
        db.session_ops
            .values()
            .filter(|op| op.id > high_water)
            .map(|op| {
                format!(
                    "op {} ({:?}) for {} [{:?}]",
                    op.id, op.kind, op.session_id, op.state
                )
            })
            .collect::<Vec<_>>()
    });
    if offenders.is_empty() {
        Ok(())
    } else {
        Err(Violation {
            invariant: "quiescence-no-op-mint",
            detail: format!(
                "a settled world minted {} new op(s) across quiet driver rounds \
                 (the enqueue/skip livelock class): {}",
                offenders.len(),
                offenders.join("; ")
            ),
        })
    }
}

/// ADR 0098 oracle #8: at quiescence every sandbox on an UP host is
/// claimed by a session row. Sandboxes on down hosts are dead state.
fn no_orphan_sandboxes(world: &SimWorld) -> Result<(), Violation> {
    let claimed = world.meta.with_db(|db| {
        db.sessions
            .values()
            .filter_map(|row| row.session.sandbox_id)
            .collect::<BTreeSet<_>>()
    });
    let hosts = world.host_world.hosts.lock();
    for (host_id, host) in hosts.iter().filter(|(_, host)| host.up) {
        for sandbox in host.sandboxes.keys() {
            if !claimed.contains(sandbox) {
                return Err(Violation {
                    invariant: "no-orphan-sandboxes",
                    detail: format!("up host {host_id} has unclaimed sandbox {sandbox}"),
                });
            }
        }
    }
    Ok(())
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
