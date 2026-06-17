//! ADR 0044 K3: operator error type. Hand-rolled `Display` + `Error`
//! impls per the project convention (no anyhow/thiserror).

use std::fmt;

/// Errors the reconcile loop can surface. The kube `Controller` requires a
/// `std::error::Error` reconcile error; the `error_policy` requeues on any
/// of these.
#[derive(Debug)]
pub enum OperatorError {
    /// Kubernetes API error (get/list/patch/delete, or client setup).
    Kube(kube::Error),
    /// A coordinator `FleetService` gRPC call failed (transport flake or a
    /// non-OK status). `op` is the logical operation (cordon/drain/…) for
    /// diagnostics — preserves the old per-op error label.
    Rpc {
        op: &'static str,
        // Boxed: `tonic::Status` is ~192 bytes; unboxed it bloats every
        // `Result<_, OperatorError>` in the reconcile loop
        // (clippy::result_large_err).
        status: Box<tonic::Status>,
    },
    /// A drain did not reach `running_sandboxes == 0` within the budget;
    /// the roll is aborted and the pod left in place.
    DrainTimeout { host_id: String, remaining: u32 },
    /// After deleting a pod for an image roll, the successor didn't come up
    /// Ready on the target image within the budget; the roll is aborted with
    /// the host still cordoned (the operator retries next reconcile).
    RollTimeout { node: String },
    /// The CR or a managed object was missing an expected field.
    Invalid(String),
    /// A cloud node-pool actuator (e.g. the GKE scaler) call failed.
    Cloud(engram_core::BackendError),
}

impl fmt::Display for OperatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kube(e) => write!(f, "kubernetes api error: {e}"),
            Self::Rpc { op, status } => write!(f, "coordinator {op} failed: {status}"),
            Self::DrainTimeout { host_id, remaining } => write!(
                f,
                "drain of host {host_id} timed out with {remaining} sandbox(es) still running"
            ),
            Self::RollTimeout { node } => write!(
                f,
                "image roll of node {node} timed out waiting for the successor pod to be Ready on target"
            ),
            Self::Invalid(msg) => write!(f, "invalid host-fleet state: {msg}"),
            Self::Cloud(e) => write!(f, "cloud scaler error: {e}"),
        }
    }
}

impl std::error::Error for OperatorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Kube(e) => Some(e),
            Self::Rpc { status, .. } => Some(status.as_ref()),
            Self::Cloud(e) => Some(e),
            _ => None,
        }
    }
}

impl From<kube::Error> for OperatorError {
    fn from(e: kube::Error) -> Self {
        Self::Kube(e)
    }
}

impl From<engram_core::BackendError> for OperatorError {
    fn from(e: engram_core::BackendError) -> Self {
        Self::Cloud(e)
    }
}
