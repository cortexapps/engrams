//! GCP cloud integrations. Today this is only the GKE node-pool
//! scaler (`gke`); the per-host `CloudBackend` metadata reader that
//! used to live here was unused and was removed.

pub mod gke;
