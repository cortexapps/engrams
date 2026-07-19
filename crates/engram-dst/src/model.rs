//! ADR 0098 R2 — the expected-state model oracle (the auditor).
//!
//! TigerBeetle's auditor shape: a model fed ONLY by ACKED workload
//! outcomes, diffed against world/SimMeta truth after every step and at
//! quiescence. Because the model is fed only by acks, a lost-response op
//! (a create whose boot never durably established, an op whose reply was
//! dropped) is LEGITIMATELY absent from the model and is never asserted on
//! — that is the honesty boundary. What the model DOES assert:
//!
//! - **acked-create/resume never silently lost** — a session the workload
//!   saw reach `Active` (its durable row committed) still has a row on
//!   every later step, unless a later ACKED destroy retired it. A row that
//!   VANISHES under a live session is exactly the #570 symptom class
//!   (coordinator-unbind vs host-teardown racing a session out of
//!   existence) and the durability-lie class this program exists to catch.
//! - **acked fields never repainted** — the image a session was created
//!   with never changes under it (read-your-acked-writes; both replicas
//!   read the same shared SimMeta, so a present row is readable on either).
//! - **acked-destroy never resurrects** (wave 4) — once a `delete_session`
//!   is ACKED (the row is terminal or already gone), the session must never
//!   be observed live again. A destroyed session that reappears in a
//!   non-terminal (reserving/live) state is a driver re-booting a torn-down
//!   session — the double-boot / orphan-backstop class the faithful-host
//!   fold-in exists to catch.
//! - **acked-rename read-your-writes** (wave 4) — a `suggested_title` we
//!   CONFIRMED materialized (read back == what the acked rename set) is
//!   never silently reverted or repainted to a different value under a live
//!   session (a resume/rebuild dropping the sticky title would be the bug).
//!   Recorded confirmed-at-write because the harness-event title path is
//!   best-effort (a 204 ack does not by itself prove the write landed).
//!
//! Non-vacuity is proven by `tests/model_oracle.rs` and
//! `tests/workload_verbs.rs`: dropping a live session's row makes the
//! create-not-lost oracle fire; resurrecting a destroyed session fires the
//! destroy oracle; repainting a confirmed title fires the rename oracle.

use std::collections::BTreeMap;

use engram_core::SessionId;

use crate::invariants::Violation;
use crate::world::SimWorld;

/// A session the workload got an acked, durably-established outcome for.
#[derive(Debug, Clone)]
struct LiveFact {
    /// The image the create/resume acked — never repainted afterward.
    image: String,
    /// A later ACKED destroy retired it; the row may now be absent (and if
    /// present must stay terminal — never resurrect live).
    destroyed: bool,
    /// The latest `suggested_title` a rename CONFIRMED materialized
    /// (read-back == set). `None` until a confirmed rename; once set it is
    /// read-your-writes — never lost or repainted under a live session.
    title: Option<String>,
}

#[derive(Default)]
pub struct ModelState {
    acked_live: BTreeMap<SessionId, LiveFact>,
}

impl ModelState {
    /// Record that the workload observed `id` reach `Active` with a durable
    /// row (an acked create or resume that fully established). Idempotent:
    /// the first ack fixes the expected image.
    pub fn record_live(&mut self, id: SessionId, image: String) {
        self.acked_live.entry(id).or_insert(LiveFact {
            image,
            destroyed: false,
            title: None,
        });
    }

    /// Record an ACKED destroy — after this the session's row may be gone,
    /// and if it is present it must stay terminal (never resurrect live).
    /// Only meaningful for a session the model already tracks as acked-live.
    pub fn record_destroy_acked(&mut self, id: SessionId) {
        if let Some(f) = self.acked_live.get_mut(&id) {
            f.destroyed = true;
        }
    }

    /// Record a CONFIRMED rename: the workload set `title` via the harness
    /// title path AND read it back materialized. Latest-wins (a later
    /// confirmed rename overwrites). Only tracked for an acked-live session
    /// (the honesty boundary — the model asserts on what it saw established).
    pub fn record_rename_acked(&mut self, id: SessionId, title: String) {
        if let Some(f) = self.acked_live.get_mut(&id) {
            f.title = Some(title);
        }
    }

    /// How many acked-live sessions the model is tracking (interestingness
    /// guard input for the tests).
    pub fn tracked(&self) -> usize {
        self.acked_live.len()
    }

    /// Diff the model against SimMeta world truth.
    pub fn check(&self, world: &SimWorld) -> Result<(), Violation> {
        world.meta.with_db(|db| {
            for (id, fact) in &self.acked_live {
                if fact.destroyed {
                    // An acked destroy legitimately removes the row (or
                    // leaves it terminal). What it must NEVER do is let the
                    // session come back to life: a present row that is
                    // non-terminal after an acked destroy is a driver
                    // re-booting a torn-down session (the double-boot class).
                    if let Some(row) = db.sessions.get(id) {
                        if !row.session.status.is_terminal() {
                            return Err(Violation {
                                invariant: "model-acked-destroy-resurrected",
                                detail: format!(
                                    "session {id} was destroyed (acked) but is live again \
                                     at {:?} — a torn-down session resurrected",
                                    row.session.status
                                ),
                            });
                        }
                    }
                    continue;
                }
                let Some(row) = db.sessions.get(id) else {
                    return Err(Violation {
                        invariant: "model-acked-create-not-lost",
                        detail: format!(
                            "session {id} reached Active (acked) but its row VANISHED \
                             with no acked destroy — a silent loss (#570 class)"
                        ),
                    });
                };
                // A live-then-terminal session keeps its row (Dead/Failed/
                // Completed are all row-present); only a delete removes it,
                // which the `destroyed` flag above accounts for.
                if row.session.image != fact.image {
                    return Err(Violation {
                        invariant: "model-acked-field-repaint",
                        detail: format!(
                            "session {id} image repainted from {:?} to {:?} \
                             (read-your-acked-writes violated)",
                            fact.image, row.session.image
                        ),
                    });
                }
                // Read-your-acked-writes for a CONFIRMED rename: the sticky
                // title never silently reverts or repaints under the live
                // session (a resume/rebuild that dropped it is the bug).
                if let Some(expected) = &fact.title {
                    if row.session.suggested_title.as_ref() != Some(expected) {
                        return Err(Violation {
                            invariant: "model-acked-rename-lost",
                            detail: format!(
                                "session {id} title was {expected:?} (confirmed acked) \
                                 but is now {:?} — read-your-acked-writes violated",
                                row.session.suggested_title
                            ),
                        });
                    }
                }
            }
            Ok(())
        })
    }
}
