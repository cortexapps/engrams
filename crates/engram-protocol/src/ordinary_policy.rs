//! Ordinary wire 38 policy boundary. Managed authority has a separate RPC contract.
use bincode::Options;
use engram_core::types::{
    egress::{
        AppEndpoint, EgressInjectEntry, EgressObserveEntry, EgressSecretEntry, SessionEgressPolicy,
    },
    image::SecretMode,
    integration::{GuestService, SessionTunnel},
};
use engram_core::{SandboxError, SandboxId, SessionId};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

// Field order is pinned to the ordinary wire 38 contract.
#[derive(Serialize, Deserialize)]
struct OrdinaryPolicy {
    session_id: SessionId,
    sandbox_id: SandboxId,
    guest_ip: Ipv4Addr,
    network_allow_hosts: Vec<String>,
    network_allow_host_patterns: Vec<String>,
    allow_all: bool,
    secrets: Vec<EgressSecretEntry>,
    injects: Vec<EgressInjectEntry>,
    observes: Vec<EgressObserveEntry>,
    guest_services: Vec<GuestService>,
    tunnels: Vec<SessionTunnel>,
    secret_mode: SecretMode,
    apps: Vec<AppEndpoint>,
}

/// Encode ordinary policy without discarding managed authority.
pub fn encode(policy: &SessionEgressPolicy) -> Result<Vec<u8>, SandboxError> {
    if policy.managed_deny.is_some() {
        return Err(SandboxError::InvalidSpec(
            "managed policy requires the managed protocol".into(),
        ));
    }
    let ordinary = OrdinaryPolicy {
        session_id: policy.session_id,
        sandbox_id: policy.sandbox_id,
        guest_ip: policy.guest_ip,
        network_allow_hosts: policy.network_allow_hosts.clone(),
        network_allow_host_patterns: policy.network_allow_host_patterns.clone(),
        allow_all: policy.allow_all,
        secrets: policy.secrets.clone(),
        injects: policy.injects.clone(),
        observes: policy.observes.clone(),
        guest_services: policy.guest_services.clone(),
        tunnels: policy.tunnels.clone(),
        secret_mode: policy.secret_mode,
        apps: policy.apps.clone(),
    };
    bincode::serialize(&ordinary)
        .map_err(|e| SandboxError::InvalidSpec(format!("encode ordinary policy: {e}")))
}

/// Decode exactly one complete ordinary policy, without managed authority.
pub fn decode(bytes: &[u8]) -> Result<SessionEgressPolicy, SandboxError> {
    let policy: OrdinaryPolicy = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(bytes.len() as u64)
        .reject_trailing_bytes()
        .deserialize(bytes)
        .map_err(|e| SandboxError::InvalidSpec(format!("decode ordinary policy: {e}")))?;
    Ok(SessionEgressPolicy {
        session_id: policy.session_id,
        sandbox_id: policy.sandbox_id,
        guest_ip: policy.guest_ip,
        network_allow_hosts: policy.network_allow_hosts,
        network_allow_host_patterns: policy.network_allow_host_patterns,
        allow_all: policy.allow_all,
        secrets: policy.secrets,
        injects: policy.injects,
        observes: policy.observes,
        guest_services: policy.guest_services,
        tunnels: policy.tunnels,
        secret_mode: policy.secret_mode,
        apps: policy.apps,
        managed_deny: None,
    })
}
