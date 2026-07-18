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
//!
//! Non-vacuity is proven by `tests/model_oracle.rs`: dropping a live
//! session's row directly makes the oracle fire; restoring it passes.

use std::collections::BTreeMap;

use engram_core::SessionId;

use crate::invariants::Violation;
use crate::world::SimWorld;

/// A session the workload got an acked, durably-established outcome for.
#[derive(Debug, Clone)]
struct LiveFact {
    /// The image the create/resume acked — never repainted afterward.
    image: String,
    /// A later ACKED destroy retired it; the row may now be absent.
    destroyed: bool,
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
        });
    }

    /// Record an ACKED destroy — after this the session's row may be gone.
    pub fn record_destroy_acked(&mut self, id: SessionId) {
        if let Some(f) = self.acked_live.get_mut(&id) {
            f.destroyed = true;
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
                    // An acked destroy legitimately removes the row.
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
            }
            Ok(())
        })
    }
}
