//! AWS cloud integrations (ADR 0122). Today this is only the ASG
//! node-pool scaler (`asg`) — the EKS twin of `engram-cloud-gcp`'s
//! GKE actuator.

pub mod asg;
