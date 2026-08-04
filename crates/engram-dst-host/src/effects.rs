//! The simulated side-effect seams (ADR 0098 Phase 2, P2).
//!
//! [`SimNbd`] and [`SimDeviceSync`] are ordering-recorded no-ops: every call
//! lands in an event log so a test can assert the sequence the flows drive
//! (P7's Flow B reattach ordering is the eventual customer). [`sim_effects`]
//! assembles the whole [`HostEffects`] bundle over the sim's
//! clock/entropy/coord and the PRODUCTION [`TokioFs`] rooted at the run's
//! tempdir — SimFs owns the directory, the honest `tokio::fs` impl does the
//! syscalls (P2 does not intercept them; that is P4's crash injector).

use std::io;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use engram_core::traits::{Clock, Entropy};
use engram_host_core::{
    DeviceSync, HostEffects, NbdConnectRequest, NbdKernel, NbdReconfigureRequest, TokioFs,
};
use parking_lot::Mutex;

use crate::coord_stub::SimCoordClient;

/// One recorded seam call, in the order the flow issued it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SeamEvent {
    NbdConnect { device: String, backend_id: String },
    NbdReconfigure { device: String, backend_id: String },
    NbdDisconnect { device: String },
    NbdBackendIdentifier { device: String },
    DeviceSync { device: String },
}

/// A shared, ordered event log both sim seams append to, so an ordering
/// assertion sees NBD ops and device syncs interleaved in issue order.
#[derive(Default)]
pub struct SeamLog {
    events: Mutex<Vec<SeamEvent>>,
}

impl SeamLog {
    pub fn record(&self, e: SeamEvent) {
        self.events.lock().push(e);
    }
}

/// [`NbdKernel`] as ordering-recorded no-ops. `connect`/`reconfigure`/
/// `disconnect` succeed and log; `backend_identifier` returns the id last
/// recorded at `connect`/`reconfigure` for the device (a single-owner
/// registry — the real kernel's `/sys/block/nbdN/backend`).
pub struct SimNbd {
    log: Arc<SeamLog>,
    backends: Mutex<std::collections::BTreeMap<String, String>>,
}

impl SimNbd {
    pub fn new(log: Arc<SeamLog>) -> Self {
        Self {
            log,
            backends: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    fn key(device: &Path) -> String {
        device.to_string_lossy().into_owned()
    }
}

#[async_trait]
impl NbdKernel for SimNbd {
    async fn connect(&self, req: NbdConnectRequest<'_>) -> io::Result<()> {
        let device = Self::key(req.device);
        self.backends
            .lock()
            .insert(device.clone(), req.backend_identifier.to_string());
        self.log.record(SeamEvent::NbdConnect {
            device,
            backend_id: req.backend_identifier.to_string(),
        });
        Ok(())
    }

    async fn reconfigure(&self, req: NbdReconfigureRequest<'_>) -> io::Result<()> {
        let device = Self::key(req.device);
        self.backends
            .lock()
            .insert(device.clone(), req.backend_identifier.to_string());
        self.log.record(SeamEvent::NbdReconfigure {
            device,
            backend_id: req.backend_identifier.to_string(),
        });
        Ok(())
    }

    async fn disconnect(&self, device: &Path) -> io::Result<()> {
        let device = Self::key(device);
        self.backends.lock().remove(&device);
        self.log.record(SeamEvent::NbdDisconnect {
            device: device.clone(),
        });
        Ok(())
    }

    fn backend_identifier(&self, device: &Path) -> Option<String> {
        let device = Self::key(device);
        self.log.record(SeamEvent::NbdBackendIdentifier {
            device: device.clone(),
        });
        self.backends.lock().get(&device).cloned()
    }

    fn connected_devices(&self) -> Vec<engram_host_core::ConnectedDevice> {
        // The seam's device registry stands in for `/sys/block/nbd*`: every
        // device with a recorded backend id is "connected". This impl exists to
        // keep the trait total — the classification barrier's inventory is built
        // by the world models (`SimHost` / `DevicePlane`) directly from their
        // generation/kernel-owner/holder state, NOT through this seam (they own
        // the pid-liveness ground truth this ZST seam does not model). Owner pid
        // is a placeholder (`1`) for the same reason. Deterministic `BTreeMap`
        // order.
        self.backends
            .lock()
            .iter()
            .map(|(device, backend_id)| engram_host_core::ConnectedDevice {
                device: std::path::PathBuf::from(device),
                owner_pid: 1,
                backend_id: Some(backend_id.clone()),
            })
            .collect()
    }
}

/// [`DeviceSync`] that records every host-page-cache sync. The real impl
/// `sync_all()`s `/dev/nbdN`; the sim just proves the flow issued the sync
/// at the right point in the shutdown ordering.
pub struct SimDeviceSync {
    log: Arc<SeamLog>,
}

impl SimDeviceSync {
    pub fn new(log: Arc<SeamLog>) -> Self {
        Self { log }
    }
}

#[async_trait]
impl DeviceSync for SimDeviceSync {
    async fn sync_device(&self, path: &Path) -> io::Result<()> {
        self.log.record(SeamEvent::DeviceSync {
            device: path.to_string_lossy().into_owned(),
        });
        Ok(())
    }
}

/// Assemble the [`HostEffects`] bundle the sim drives flows through: the
/// sim clock/entropy, the [`SimCoordClient`], the production [`TokioFs`]
/// (SimFs owns the directory it writes into), and the recording NBD/device
/// seams. Returns the shared [`SeamLog`] too so tests can assert ordering.
pub fn sim_effects(
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
    coord: Arc<SimCoordClient>,
) -> (HostEffects, Arc<SeamLog>) {
    let log = Arc::new(SeamLog::default());
    let effects = HostEffects::production(
        clock,
        entropy,
        coord,
        Arc::new(TokioFs),
        Arc::new(SimDeviceSync::new(log.clone())),
        Arc::new(SimNbd::new(log.clone())),
    );
    (effects, log)
}
