//! Egress-policy types used by `SandboxBackend::notify_session_policy`
//! and the wire-protocol `NotifyKind::SessionEgressPolicy` frame.
//!
//! These intentionally use flat `Vec<String>` for host allow-lists
//! rather than the `engram-egress-proxy::HostList` type — the proxy
//! crate builds a matcher-friendly representation locally on
//! receipt. Keeping the cross-crate types matcher-agnostic means
//! `engram-core` doesn't pull in policy-matcher dependencies.
//!
//! ADR 0006.

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use crate::types::image::SecretMode;
use crate::{SandboxId, SessionId};

/// Per-session egress policy the host-agent's proxy registers
/// against a session's `guest_ip`. Built by the coordinator from
/// the image manifest + resolved secret bundle, shipped to the
/// owning host over the WS, applied locally by the host-agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionEgressPolicy {
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    /// IP the guest VM presents on the host-side tap interface.
    /// The proxy's registry indexes by this; iptables REDIRECT
    /// preserves source IP.
    pub guest_ip: Ipv4Addr,
    /// Hostnames the manifest's `[network].allow_hosts` permits.
    pub network_allow_hosts: Vec<String>,
    /// Glob patterns from `[network].allow_host_patterns`.
    pub network_allow_host_patterns: Vec<String>,
    /// Per-secret entries (placeholder → real_value with per-secret
    /// host allow-list). Empty for `SecretMode::Literal` images.
    pub secrets: Vec<EgressSecretEntry>,
    /// Image's secret delivery mode. The proxy uses this to decide
    /// whether to MITM (`Broker`) or just SNI-filter (`Literal`).
    pub secret_mode: SecretMode,
}

/// One secret's substitution policy. `placeholder` is the value the
/// guest sees in its env (`engram_ph_<session>_<hash>`); the proxy
/// replaces it with `real_value` in outbound HTTPS traffic when the
/// destination SNI matches `allow_hosts` / `allow_host_patterns`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EgressSecretEntry {
    pub placeholder: String,
    pub real_value: String,
    pub allow_hosts: Vec<String>,
    pub allow_host_patterns: Vec<String>,
}
