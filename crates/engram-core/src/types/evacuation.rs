//! ADR 0018: types for the session-evacuation primitive.
//!
//! Evacuation is the "move a sandbox from its current host to a peer"
//! operation that turns `Sandbox` into a host-fungible value per ADR
//! 0015 §M4. The `sandbox_id` token in the registry / DB is
//! substitutable across an evac — new id, same logical session.
//!
//! Lives in `engram-core::types` because both the trait surface
//! (`HostClient::evacuate`) and the orchestrator (`HostRegistry`) need
//! the receipt + loss shape, and they live in different crates.

use serde::{Deserialize, Serialize};

use super::ids::{HostId, SandboxId};

/// Outcome of a successful evacuation. The caller usually only cares
/// about `new_sandbox_id` (cache invalidation) and `loss` (telemetry).
/// `new_host_id` is exposed so admin endpoints can echo the placement
/// decision back to the operator.
///
/// `session_id` is intentionally NOT part of the receipt: the
/// `session_id` is the stable handle across an evac per ADR 0015 §M4
/// ("the `sandbox_id` token can be substituted, but the same logical
/// Sandbox from the session's perspective"). The caller already knows
/// it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvacReceipt {
    pub new_host_id: HostId,
    pub new_sandbox_id: SandboxId,
    pub loss: EvacLoss,
}

/// What, if anything, was sacrificed by the evacuation. The graceful-
/// drain path (alive source) is always `None`; the dead-source paths
/// degrade as needed per ADR 0016 §"What gets easier".
///
/// The `reason` field on `Memory` is conventionally one of the short
/// kebab-case tags the orchestrator emits ("source-dead-no-snapshot",
/// "source-disk-only-available", etc.). Operators key alerts on it; new
/// reasons added by future loss paths should be documented at the call
/// site that produces them.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvacLoss {
    /// Memory + disk both preserved. Alive-source drain path.
    None,
    /// Memory state was not recoverable; disk was restored from
    /// `sessions.live_disk_manifest_*` (ADR 0016 Phase B) or from the
    /// most recent recoverable snapshot's disk manifest.
    Memory { reason: String },
}

impl EvacLoss {
    /// Stable tag used in metrics / log fields. Avoids the verbose
    /// JSON form when only the discriminant matters.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Memory { .. } => "memory",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::{HostId, SandboxId};

    #[test]
    fn evac_loss_as_str_matches_discriminant() {
        assert_eq!(EvacLoss::None.as_str(), "none");
        assert_eq!(
            EvacLoss::Memory {
                reason: "source-dead".into()
            }
            .as_str(),
            "memory"
        );
    }

    #[test]
    fn evac_loss_round_trips_through_json() {
        // The wire shape is the load-bearing contract — admin endpoint
        // responses and telemetry both depend on the `kind` tag layout.
        let none = serde_json::to_value(EvacLoss::None).unwrap();
        assert_eq!(none, serde_json::json!({"kind": "none"}));
        let mem = serde_json::to_value(EvacLoss::Memory {
            reason: "source-dead-no-snapshot".into(),
        })
        .unwrap();
        assert_eq!(
            mem,
            serde_json::json!({"kind": "memory", "reason": "source-dead-no-snapshot"})
        );

        let back: EvacLoss = serde_json::from_value(serde_json::json!({"kind": "none"})).unwrap();
        assert_eq!(back, EvacLoss::None);
    }

    #[test]
    fn evac_receipt_round_trips_through_json() {
        let receipt = EvacReceipt {
            new_host_id: HostId::new(),
            new_sandbox_id: SandboxId::new(),
            loss: EvacLoss::Memory {
                reason: "source-dead".into(),
            },
        };
        let blob = serde_json::to_string(&receipt).unwrap();
        let back: EvacReceipt = serde_json::from_str(&blob).unwrap();
        assert_eq!(back, receipt);
    }
}
