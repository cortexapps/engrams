//! A sandbox's guest-network identity, as one coherent value.
//!
//! Replaces the retired `SandboxBackend::{guest_ip, netns_name_for,
//! vm_internal_ip}` accessor triple (the "three-IP accident"): three
//! sibling methods whose combined doc comment existed solely to explain
//! which one a caller must pick, and whose distinction is real FC network
//! mechanism (netns SNAT slot vs. in-VM eth0 address vs. the namespace to
//! dial from) that used to leak through the seam as prose instead of
//! types. See `SandboxBackend::guest_endpoints`.

/// A sandbox's guest-network identity, as one coherent value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestEndpoints {
    /// The IPv4 the HOST sees this guest's traffic as — the netns SNAT
    /// pool slot for warm-restored FC sandboxes, the TAP /30 guest IP
    /// otherwise. This is the egress-proxy registry key and the value
    /// that goes into `SessionEgressPolicy.guest_ip`. (was: `guest_ip()`)
    pub egress_identity: std::net::Ipv4Addr,
    /// The IPv4 to DIAL for a direct host->guest TCP connection — the
    /// in-VM eth0 address. Equals `egress_identity` when there is no
    /// netns/SNAT indirection (cold FC, VZ, Process). (was: `vm_internal_ip()`)
    pub dial_ip: std::net::Ipv4Addr,
    /// Per-VM Linux netns the sandbox's TAP lives in, if any
    /// (`engr-vm-<id>`, warm-restored FC only). Post-ADR-0066 no dial
    /// path enters it; kept for diagnostics/tests and teardown parity.
    /// (was: `netns_name_for()`)
    pub netns: Option<String>,
    /// Host-side agentd vsock UDS path (FC: the firecracker vsock UDS;
    /// VZ: the bridge UDS stem), when the backend has one. Informational
    /// today (diagnostics; no production consumer at introduction).
    pub vsock_uds: Option<std::path::PathBuf>,
}
