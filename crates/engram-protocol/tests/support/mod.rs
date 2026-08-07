//! Shared valid-value proptest strategies for the coord ↔ host-agent bincode
//! payloads (`engram_protocol::wire`), consumed by BOTH property suites:
//!   - `decode_never_panics.rs` (ADR 0099 §H4) — mutates these valid frames;
//!   - `codec_roundtrip.rs` (ADR 0099 §H3) — asserts encode→decode identity.
//!
//! Each consumer links a subset, so the module carries a narrow
//! `allow(dead_code)`. Both covered types are structs (no variant enums), so
//! there is no exhaustiveness guard to add — a new struct FIELD is caught by
//! the round-trip property itself (a field the encoder writes but the strategy
//! leaves defaulted still round-trips; the guard is `wire_golden` for a
//! non-trailing insert).
#![allow(dead_code)]

use std::collections::HashMap;

use engram_protocol::wire::{WireExecRequest, WireReapStats};
use proptest::prelude::*;

pub fn s() -> impl Strategy<Value = String> {
    "[ -~]{0,10}"
}

pub fn small_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..=16)
}

pub fn wire_exec_request() -> impl Strategy<Value = WireExecRequest> {
    (
        proptest::collection::vec(s(), 0..3),
        proptest::option::of(small_bytes()),
        proptest::option::of((s(), s())).prop_map(|kv| kv.into_iter().collect::<HashMap<_, _>>()),
        proptest::option::of(s()),
        proptest::option::of(any::<u64>()),
        proptest::option::of(s()),
        proptest::option::of(any::<u64>()),
        proptest::option::of(any::<u64>()),
        proptest::option::of(any::<bool>()),
    )
        .prop_map(
            |(
                command,
                stdin,
                env,
                workdir,
                timeout_ms,
                exec_id,
                stdout_offset,
                stderr_offset,
                wake,
            )| WireExecRequest {
                command,
                stdin,
                env,
                workdir,
                timeout_ms,
                exec_id,
                stdout_offset,
                stderr_offset,
                wake,
            },
        )
}

pub fn wire_reap_stats() -> impl Strategy<Value = WireReapStats> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(files_scanned, files_deleted, bytes_freed, unparseable, too_young)| WireReapStats {
                files_scanned,
                files_deleted,
                bytes_freed,
                files_skipped_unparseable: unparseable,
                files_skipped_too_young: too_young,
            },
        )
}

// ── SessionEgressPolicy (ADR 0006 / 0056 / 0109) ─────────────────────────────
//
// The policy that carries the session's credentials and its whole permission
// boundary to the host. It rides `NotifyKind::SessionEgressPolicy` as bincode,
// and #931 showed what a decode failure here costs: an internally-tagged
// `CredentialMintSource` encoded fine and could never be DECODED, so every
// session with a minted inject failed at boot.
//
// The strategy below carries an EXHAUSTIVENESS GUARD (AGENTS.md): a
// wildcard-free `match` over `CredentialMintSource`. A new variant — an AWS or
// Azure mint authority, say — is then a compile error here rather than a
// silent hole in the corpus.

use chrono::{DateTime, Utc};
use engram_core::types::egress::{
    EgressInjectEntry, EgressObserveEntry, EgressSecretEntry, SessionEgressPolicy,
};
use engram_core::types::image::SecretMode;
use engram_core::types::integration::{CredentialMintSource, GuestService, ObserveUrlFallback};
use engram_core::{SandboxId, SessionId};

/// Every shape a `CredentialMintSource` can take.
///
/// The `match` is deliberately wildcard-free. Adding a variant without adding
/// it here does not compile, so the round-trip corpus cannot quietly stop
/// covering a mint authority.
pub fn credential_mint_source() -> impl Strategy<Value = CredentialMintSource> {
    fn shapes(sample: CredentialMintSource) -> BoxedStrategy<CredentialMintSource> {
        match sample {
            CredentialMintSource::Connection { .. } => (s(), s())
                .prop_map(
                    |(connection_id, provider)| CredentialMintSource::Connection {
                        connection_id,
                        provider,
                    },
                )
                .boxed(),
            CredentialMintSource::OauthConnector { .. } => (s(), s())
                .prop_map(
                    |(connection_id, provider)| CredentialMintSource::OauthConnector {
                        connection_id,
                        provider,
                    },
                )
                .boxed(),
        }
    }
    prop_oneof![
        shapes(CredentialMintSource::Connection {
            connection_id: String::new(),
            provider: String::new(),
        }),
        shapes(CredentialMintSource::OauthConnector {
            connection_id: String::new(),
            provider: String::new(),
        }),
    ]
}

/// Path globs cover all three matcher forms the proxy understands, including
/// the `segment-path:` form Google Cloud policies use (a `*` stops at `/`, and
/// the query string is excluded).
fn path_globs() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec(
        prop_oneof![
            s(),
            s().prop_map(|value| format!("segment:{value}")),
            s().prop_map(|value| format!("segment-path:{value}")),
        ],
        0..3,
    )
}

fn timestamp() -> impl Strategy<Value = DateTime<Utc>> {
    (0_i64..2_000_000_000).prop_map(|secs| DateTime::from_timestamp(secs, 0).expect("in range"))
}

fn egress_secret_entry() -> impl Strategy<Value = EgressSecretEntry> {
    (
        s(),
        s(),
        proptest::collection::vec(s(), 0..3),
        proptest::collection::vec(s(), 0..3),
    )
        .prop_map(
            |(placeholder, real_value, allow_hosts, allow_host_patterns)| EgressSecretEntry {
                placeholder,
                real_value,
                allow_hosts,
                allow_host_patterns,
            },
        )
}

fn egress_inject_entry() -> impl Strategy<Value = EgressInjectEntry> {
    (
        (s(), s(), s()),
        (
            proptest::collection::vec(s(), 0..3),
            proptest::collection::vec(s(), 0..3),
        ),
        (proptest::collection::vec(s(), 0..3), path_globs()),
        (s(), s()),
        (
            proptest::option::of(credential_mint_source()),
            proptest::option::of(timestamp()),
        ),
    )
        .prop_map(
            |(
                (secret, header_name, header_template),
                (allow_hosts, allow_host_patterns),
                (methods, path_globs),
                (graphql_operation, graphql_field),
                (mint_source, expires_at),
            )| EgressInjectEntry {
                secret,
                header_name,
                header_template,
                allow_hosts,
                allow_host_patterns,
                methods,
                path_globs,
                graphql_operation,
                graphql_field,
                mint_source,
                expires_at,
            },
        )
}

fn egress_observe_entry() -> impl Strategy<Value = EgressObserveEntry> {
    (
        (
            proptest::collection::vec(s(), 0..3),
            proptest::collection::vec(s(), 0..3),
        ),
        (proptest::collection::vec(s(), 0..3), path_globs()),
        (s(), s(), s()),
        (proptest::option::of(s()), any::<bool>()),
        (s(), s()),
        (
            proptest::collection::vec((s(), s()), 0..3),
            proptest::option::of(s()),
        ),
        proptest::option::of(
            (s(), proptest::collection::vec((s(), s()), 0..2))
                .prop_map(|(pattern, fields)| ObserveUrlFallback { pattern, fields }),
        ),
    )
        .prop_map(
            |(
                (allow_hosts, allow_host_patterns),
                (methods, path_globs),
                (provider, asset_kind, surface),
                (success_status_class, success_no_graphql_errors),
                (graphql_operation, graphql_field),
                (data, fetchable),
                url_fallback,
            )| EgressObserveEntry {
                allow_hosts,
                allow_host_patterns,
                methods,
                path_globs,
                provider,
                asset_kind,
                surface,
                success_status_class,
                success_no_graphql_errors,
                graphql_operation,
                graphql_field,
                data,
                fetchable,
                url_fallback,
            },
        )
}

/// Registered guest services, including the empty set.
pub fn guest_services() -> impl Strategy<Value = Vec<GuestService>> {
    proptest::collection::vec(
        prop_oneof![
            Just(GuestService::new("gcp.gce_metadata")),
            Just(GuestService::new("test.service")),
        ],
        0..3,
    )
}

fn secret_mode() -> impl Strategy<Value = SecretMode> {
    fn shapes(sample: SecretMode) -> BoxedStrategy<SecretMode> {
        // Wildcard-free: a new delivery mode must be added here too.
        match sample {
            SecretMode::Literal | SecretMode::Broker => {
                prop_oneof![Just(SecretMode::Literal), Just(SecretMode::Broker)].boxed()
            }
        }
    }
    shapes(SecretMode::Literal)
}

pub fn session_egress_policy() -> impl Strategy<Value = SessionEgressPolicy> {
    (
        (any::<u128>(), any::<u128>(), any::<[u8; 4]>()),
        (
            proptest::collection::vec(s(), 0..3),
            proptest::collection::vec(s(), 0..3),
            any::<bool>(),
        ),
        (
            proptest::collection::vec(egress_secret_entry(), 0..2),
            proptest::collection::vec(egress_inject_entry(), 0..2),
            proptest::collection::vec(egress_observe_entry(), 0..2),
        ),
        (guest_services(), secret_mode()),
    )
        .prop_map(
            |(
                (session, sandbox, ip),
                (network_allow_hosts, network_allow_host_patterns, allow_all),
                (secrets, injects, observes),
                (guest_services, secret_mode),
            )| SessionEgressPolicy {
                session_id: SessionId::from(uuid::Uuid::from_u128(session)),
                sandbox_id: SandboxId::from(uuid::Uuid::from_u128(sandbox)),
                guest_ip: std::net::Ipv4Addr::from(ip),
                network_allow_hosts,
                network_allow_host_patterns,
                allow_all,
                secrets,
                injects,
                observes,
                guest_services,
                tunnels: Vec::new(),
                secret_mode,
            },
        )
}
