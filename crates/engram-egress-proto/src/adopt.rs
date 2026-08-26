//! The pure adopt decision (ADR 0121).
//!
//! `ensure_proxyd` gathers evidence — the manifest, the live `/proc`
//! identity, a `Hello` round-trip, the accept-loop probe — and this
//! module decides what to do with it. Pure function, truth-table
//! tested, no I/O (the `spawn()`/`run_once()` idiom).

use crate::manifest::{ProcIdentity, ProxydManifest};
use crate::HelloInfo;

/// What `ensure_proxyd` should do with the daemon slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdoptPlan {
    /// A healthy daemon of the expected build and config is serving:
    /// keep it. This is the roll path — streams survive.
    Adopt,
    /// A healthy daemon of a DIFFERENT build or config is serving:
    /// ask it to exit (graceful `Shutdown` op), then spawn ours.
    RestartForUpgrade,
    /// The manifest's process exists but is not trustworthy (identity
    /// verified, control socket dead or probe failed — a wedged
    /// daemon): kill the pid, remove the manifest, spawn fresh.
    /// Adopting a wedged daemon would recreate the ADR 0083 fail-open
    /// incident shape — a green host serving RSTs.
    KillStaleAndSpawn,
    /// Nothing live to consider (no manifest, or its process is gone
    /// or recycled): spawn fresh.
    SpawnFresh,
}

/// What the calling host-agent expects a kept daemon to match.
#[derive(Clone, Debug)]
pub struct ExpectedProxyd<'a> {
    /// Our own build-time fingerprint. `None` (a local build) never
    /// adopts — replace rather than guess.
    pub source_fingerprint: Option<&'a str>,
    pub proxy_port: u16,
    pub dns_port: u16,
    pub gateway_port: u16,
    /// SHA-256 (hex) of the CA cert PEM we would spawn with.
    pub ca_fingerprint: &'a str,
    pub coord_url: &'a str,
}

/// The evidence `ensure_proxyd` gathered about the live slot.
#[derive(Clone, Debug, Default)]
pub struct LiveEvidence {
    /// Three-axis `/proc` identity of the manifest pid, when readable.
    /// `None` on non-Linux (dev) and when the process is gone — the
    /// hello outcome then carries the liveness question alone.
    pub identity: Option<ProcIdentity>,
    /// A `Hello` round-trip on the manifest's control socket.
    pub hello: Option<HelloInfo>,
    /// The accept-loop probe: a TCP connect to the proxy port was
    /// accepted AND promptly closed (the registry `NoSession` drop),
    /// proving the dispatch path runs — a kernel backlog handshake
    /// alone proves nothing.
    pub probe_ok: bool,
}

/// Decide the daemon slot. See the truth table in the tests.
pub fn decide_adopt(
    manifest: Option<&ProxydManifest>,
    evidence: &LiveEvidence,
    expected: &ExpectedProxyd<'_>,
) -> AdoptPlan {
    let Some(manifest) = manifest else {
        // No record of a daemon. A live orphan without a manifest is
        // possible only via a torn write we already treat as absent;
        // its listeners would collide with the fresh spawn's bind and
        // surface loudly there (bind_with_retry → fail-closed).
        return AdoptPlan::SpawnFresh;
    };

    // Identity check (Linux). A recycled pid — or a dead one — means
    // there is nothing of ours to kill: spawn fresh.
    let identity_verified = match &evidence.identity {
        Some(live) => {
            if !live.matches(manifest) {
                return AdoptPlan::SpawnFresh;
            }
            true
        }
        None => false,
    };

    let Some(hello) = &evidence.hello else {
        // Manifest exists, control socket dead/unresponsive.
        return if identity_verified {
            // The recorded process is provably alive but not
            // answering: wedged. Kill it.
            AdoptPlan::KillStaleAndSpawn
        } else {
            // Non-Linux (no /proc), or the process cannot be read:
            // nothing provably alive to kill — spawn fresh; a truly
            // live-but-mute orphan surfaces as a bind conflict.
            AdoptPlan::SpawnFresh
        };
    };

    // From here a daemon is answering on the control socket. Config
    // and build must match exactly to adopt; any mismatch is a
    // deliberate, graceful replacement.
    if hello.proto_version != crate::PROTO_VERSION {
        // We cannot trust our `Shutdown` encoding against an alien
        // protocol; replace by signal.
        return AdoptPlan::KillStaleAndSpawn;
    }
    let fingerprints_match = match (expected.source_fingerprint, &hello.source_fingerprint) {
        (Some(ours), Some(theirs)) => ours == theirs,
        // Either side lacking a fingerprint (local/dev builds) never
        // adopts — replace rather than guess (ADR 0121).
        _ => false,
    };
    if !fingerprints_match
        || hello.proxy_port != expected.proxy_port
        || hello.dns_port != expected.dns_port
        || hello.gateway_port != expected.gateway_port
        || hello.ca_fingerprint != expected.ca_fingerprint
        || hello.coord_url != expected.coord_url
    {
        return AdoptPlan::RestartForUpgrade;
    }
    if !evidence.probe_ok {
        // Control plane answers but the accept loop does not: wedged.
        return AdoptPlan::KillStaleAndSpawn;
    }
    AdoptPlan::Adopt
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const FP: &str = "fingerprint-a";
    const CA: &str = "ca-fp";
    const COORD: &str = "http://coord:8080";

    fn manifest() -> ProxydManifest {
        ProxydManifest {
            schema_version: crate::manifest::MANIFEST_SCHEMA_VERSION,
            pid: 100,
            start_time_jiffies: 5,
            comm: "engram-egress-p".into(),
            source_fingerprint: Some(FP.into()),
            proxy_port: 8443,
            dns_port: 5353,
            gateway_port: 13338,
            control_sock: PathBuf::from("/w/egress-proxyd.sock"),
        }
    }

    fn identity() -> ProcIdentity {
        ProcIdentity {
            pid: 100,
            start_time_jiffies: 5,
            comm: "engram-egress-p".into(),
        }
    }

    fn hello() -> HelloInfo {
        HelloInfo {
            proto_version: crate::PROTO_VERSION,
            source_fingerprint: Some(FP.into()),
            proxy_port: 8443,
            dns_port: 5353,
            gateway_port: 13338,
            ca_fingerprint: CA.into(),
            coord_url: COORD.into(),
        }
    }

    fn expected() -> ExpectedProxyd<'static> {
        ExpectedProxyd {
            source_fingerprint: Some(FP),
            proxy_port: 8443,
            dns_port: 5353,
            gateway_port: 13338,
            ca_fingerprint: CA,
            coord_url: COORD,
        }
    }

    fn evidence(
        identity: Option<ProcIdentity>,
        hello: Option<HelloInfo>,
        probe_ok: bool,
    ) -> LiveEvidence {
        LiveEvidence {
            identity,
            hello,
            probe_ok,
        }
    }

    #[test]
    fn fresh_node_spawns() {
        let plan = decide_adopt(None, &LiveEvidence::default(), &expected());
        assert_eq!(plan, AdoptPlan::SpawnFresh);
    }

    #[test]
    fn healthy_roll_adopts() {
        let plan = decide_adopt(
            Some(&manifest()),
            &evidence(Some(identity()), Some(hello()), true),
            &expected(),
        );
        assert_eq!(plan, AdoptPlan::Adopt);
    }

    #[test]
    fn fingerprint_mismatch_restarts_for_upgrade() {
        let mut h = hello();
        h.source_fingerprint = Some("fingerprint-b".into());
        let plan = decide_adopt(
            Some(&manifest()),
            &evidence(Some(identity()), Some(h), true),
            &expected(),
        );
        assert_eq!(plan, AdoptPlan::RestartForUpgrade);
    }

    #[test]
    fn local_builds_without_fingerprints_never_adopt() {
        // Ours missing.
        let mut exp = expected();
        exp.source_fingerprint = None;
        let plan = decide_adopt(
            Some(&manifest()),
            &evidence(Some(identity()), Some(hello()), true),
            &exp,
        );
        assert_eq!(plan, AdoptPlan::RestartForUpgrade);
        // Theirs missing.
        let mut h = hello();
        h.source_fingerprint = None;
        let plan = decide_adopt(
            Some(&manifest()),
            &evidence(Some(identity()), Some(h), true),
            &expected(),
        );
        assert_eq!(plan, AdoptPlan::RestartForUpgrade);
    }

    #[test]
    fn dead_pid_spawns_fresh() {
        // /proc read failed → identity None, and (being dead) no hello.
        let plan = decide_adopt(Some(&manifest()), &evidence(None, None, false), &expected());
        assert_eq!(plan, AdoptPlan::SpawnFresh);
    }

    #[test]
    fn recycled_pid_spawns_fresh_without_killing() {
        let mut live = identity();
        live.start_time_jiffies = 999; // same pid, different life
        let plan = decide_adopt(
            Some(&manifest()),
            &evidence(Some(live), None, false),
            &expected(),
        );
        assert_eq!(plan, AdoptPlan::SpawnFresh);
    }

    #[test]
    fn recycled_comm_spawns_fresh() {
        let mut live = identity();
        live.comm = "not-our-daemon".into();
        let plan = decide_adopt(
            Some(&manifest()),
            &evidence(Some(live), None, false),
            &expected(),
        );
        assert_eq!(plan, AdoptPlan::SpawnFresh);
    }

    #[test]
    fn verified_alive_but_mute_is_killed() {
        let plan = decide_adopt(
            Some(&manifest()),
            &evidence(Some(identity()), None, false),
            &expected(),
        );
        assert_eq!(plan, AdoptPlan::KillStaleAndSpawn);
    }

    #[test]
    fn answering_but_probe_dead_is_killed() {
        // The wedged-accept-loop shape: control plane answers, data
        // plane does not. Adopting it would be the ADR 0083 fail-open.
        let plan = decide_adopt(
            Some(&manifest()),
            &evidence(Some(identity()), Some(hello()), false),
            &expected(),
        );
        assert_eq!(plan, AdoptPlan::KillStaleAndSpawn);
    }

    #[test]
    fn proto_version_mismatch_is_killed_by_signal() {
        let mut h = hello();
        h.proto_version = crate::PROTO_VERSION + 1;
        let plan = decide_adopt(
            Some(&manifest()),
            &evidence(Some(identity()), Some(h), true),
            &expected(),
        );
        assert_eq!(plan, AdoptPlan::KillStaleAndSpawn);
    }

    #[test]
    fn port_config_change_restarts_for_upgrade() {
        let mut exp = expected();
        exp.proxy_port = 9443;
        let plan = decide_adopt(
            Some(&manifest()),
            &evidence(Some(identity()), Some(hello()), true),
            &exp,
        );
        assert_eq!(plan, AdoptPlan::RestartForUpgrade);
    }

    #[test]
    fn ca_rotation_restarts_for_upgrade() {
        let mut exp = expected();
        exp.ca_fingerprint = "rotated-ca";
        let plan = decide_adopt(
            Some(&manifest()),
            &evidence(Some(identity()), Some(hello()), true),
            &exp,
        );
        assert_eq!(plan, AdoptPlan::RestartForUpgrade);
    }

    #[test]
    fn coord_url_change_restarts_for_upgrade() {
        let mut exp = expected();
        exp.coord_url = "http://other-coord:8080";
        let plan = decide_adopt(
            Some(&manifest()),
            &evidence(Some(identity()), Some(hello()), true),
            &exp,
        );
        assert_eq!(plan, AdoptPlan::RestartForUpgrade);
    }

    #[test]
    fn dev_socket_liveness_adopt_path_still_requires_fingerprints() {
        // Non-Linux: identity None but hello answered. With matching
        // fingerprints this WOULD adopt (socket-liveness adopt)…
        let plan = decide_adopt(
            Some(&manifest()),
            &evidence(None, Some(hello()), true),
            &expected(),
        );
        assert_eq!(plan, AdoptPlan::Adopt);
        // …but dev builds have no fingerprint, so dev in practice
        // replaces (covered by local_builds_without_fingerprints_*).
    }
}
