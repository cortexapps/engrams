//! ADR 0099 H3 (PR 2): codec ROUND-TRIP identity for the host ↔ in-guest
//! agentd vsock wire (`engram_agentd::proto`).
//!
//! Complement to `decode_never_panics.rs` (H4) and `wire_golden.rs`: goldens
//! pin the bytes of one hand-picked value per type, decode-never-panics proves
//! garbage never panics, and these round-trips prove `encode → decode ==
//! identity` across the whole valid-value space. That is what catches an
//! encode/decode asymmetry on a NEW field/variant — an APPEND-ONLY `WireRequest`
//! or `WireResponse` addition that the encoder writes but the decoder drops —
//! which a single golden cannot.
//!
//! Every public frame/message type is covered. Valid-value strategies are
//! shared with the decode suite via `tests/support/mod.rs`.

use engram_agentd::proto::{
    AgentReady, SpawnHarnessRequest, WireDownloadResponse, WireExecEvent, WireExecRequest,
    WireFileChunk, WireHandshake, WireHandshakeAck, WireRequest, WireResponse, WireStatResponse,
};
use proptest::prelude::*;

mod support;

/// One round-trip property per wire type: arbitrary valid value → bincode
/// encode → decode == original. 128 cases keeps the suite well under the
/// 3-minute nextest slow-timeout.
macro_rules! roundtrip {
    ($name:ident, $ty:ty, $strat:expr) => {
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(128))]
            #[test]
            fn $name(value in $strat) {
                let bytes = bincode::serialize(&value).expect("encode");
                let decoded: $ty = bincode::deserialize(&bytes).expect("decode");
                prop_assert_eq!(decoded, value);
            }
        }
    };
}

// Request/response envelopes + their bodies.
roundtrip!(
    wire_request_roundtrips,
    WireRequest,
    support::wire_request()
);
roundtrip!(
    wire_response_roundtrips,
    WireResponse,
    support::wire_response()
);
roundtrip!(
    wire_exec_event_roundtrips,
    WireExecEvent,
    support::wire_exec_event()
);
roundtrip!(
    wire_exec_request_roundtrips,
    WireExecRequest,
    support::wire_exec_request()
);
roundtrip!(
    spawn_harness_roundtrips,
    SpawnHarnessRequest,
    support::spawn_harness()
);
roundtrip!(
    wire_stat_response_roundtrips,
    WireStatResponse,
    support::wire_stat_response()
);
roundtrip!(
    wire_download_response_roundtrips,
    WireDownloadResponse,
    support::wire_download_response()
);

// Handshake + readiness frames.
roundtrip!(
    wire_handshake_roundtrips,
    WireHandshake,
    support::wire_handshake()
);
roundtrip!(
    wire_handshake_ack_roundtrips,
    WireHandshakeAck,
    support::wire_handshake_ack()
);
roundtrip!(agent_ready_roundtrips, AgentReady, support::agent_ready());
roundtrip!(
    wire_file_chunk_roundtrips,
    WireFileChunk,
    support::wire_file_chunk()
);
