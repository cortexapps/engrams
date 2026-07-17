//! [`HostEffects`] — the world-input bundle the host-agent's lifecycle
//! flows reach through (ADR 0098 Phase 2).

use std::sync::Arc;

use engram_core::traits::{Clock, Entropy};

use crate::coord::CoordControlPlane;
use crate::device::DeviceSync;
use crate::fs::HostFs;
use crate::nbd::NbdKernel;

/// The host-agent's injected world, mirroring the coordinator's `Services`.
/// A **bundle struct, not a mega-trait**: a god trait would recreate the
/// mock-zoo that ADR 0098 D4 retired (see the crate-level doc). Each field
/// is an `Arc<dyn …>` so a flow holds a cheap clone and the simulator swaps
/// any one seam independently.
#[derive(Clone)]
pub struct HostEffects {
    pub clock: Arc<dyn Clock>,
    pub entropy: Arc<dyn Entropy>,
    pub coord: Arc<dyn CoordControlPlane>,
    pub fs: Arc<dyn HostFs>,
    pub device: Arc<dyn DeviceSync>,
    pub nbd: Arc<dyn NbdKernel>,
}

impl HostEffects {
    /// Assemble a bundle from concrete seam impls. The production
    /// constructor wires `SystemClock`/`OsEntropy` and the reqwest
    /// `HttpCoordClient` / `TokioFs` / the host-agent's device+kernel
    /// impls; the simulator wires its `Sim*` counterparts.
    pub fn production(
        clock: Arc<dyn Clock>,
        entropy: Arc<dyn Entropy>,
        coord: Arc<dyn CoordControlPlane>,
        fs: Arc<dyn HostFs>,
        device: Arc<dyn DeviceSync>,
        nbd: Arc<dyn NbdKernel>,
    ) -> Self {
        Self {
            clock,
            entropy,
            coord,
            fs,
            device,
            nbd,
        }
    }
}
