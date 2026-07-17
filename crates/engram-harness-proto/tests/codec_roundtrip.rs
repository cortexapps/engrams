//! ADR 0099 H3 (PR 2): codec ROUND-TRIP identity for the harness ↔ host
//! vsock wire (`engram-harness-proto`).
//!
//! The complement to `decode_never_panics.rs` (H4) and `wire_golden.rs`:
//!   - goldens pin the exact BYTES a known value encodes to, so they catch a
//!     format break across versions;
//!   - decode-never-panics proves garbage in never panics;
//!   - these round-trips prove `encode → decode == identity` for arbitrary
//!     VALID values. That is the property goldens miss: a NEW field or variant
//!     added with an encode/decode asymmetry (e.g. a trailing field the
//!     encoder writes but the decoder skips, or a `serde(default)` shadowing a
//!     real value) passes the golden — the golden fixes one hand-picked value
//!     — yet loses data on the general population. This suite fuzzes the whole
//!     valid-value space, so such an asymmetry shrinks to a checked-in
//!     counterexample.
//!
//! Every public frame/message type is covered. The valid-value strategies are
//! shared with the decode suite via `tests/support/mod.rs`.

use engram_harness_proto::{
    AgentRole, AttachReject, CheckpointAck, CheckpointReason, FileChange, ForgeRequest,
    ForgeResponse, HarnessAttach, HarnessAttachAck, HarnessCommand, HarnessEvent, HarnessFrame,
    RelayAck, RelayConnect, UploadRequest, UploadResponse,
};
use proptest::prelude::*;

mod support;

/// Generate one round-trip property per wire type: arbitrary valid value →
/// bincode encode → decode, asserting the decoded value equals the original.
/// 128 cases keeps every suite well under the 3-minute nextest slow-timeout
/// (each case is a small serialize + deserialize).
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

// Steady-state duplex frame + its component enums (each also round-tripped
// standalone since they are public and cross the wire directly).
roundtrip!(
    harness_frame_roundtrips,
    HarnessFrame,
    support::harness_frame()
);
roundtrip!(
    harness_event_roundtrips,
    HarnessEvent,
    support::harness_event()
);
roundtrip!(
    harness_command_roundtrips,
    HarnessCommand,
    support::harness_command()
);
roundtrip!(file_change_roundtrips, FileChange, support::file_change());
roundtrip!(agent_role_roundtrips, AgentRole, support::agent_role());

// Handshake.
roundtrip!(
    harness_attach_roundtrips,
    HarnessAttach,
    support::harness_attach()
);
roundtrip!(
    harness_attach_ack_roundtrips,
    HarnessAttachAck,
    support::harness_attach_ack()
);
roundtrip!(
    attach_reject_roundtrips,
    AttachReject,
    support::attach_reject()
);

// Checkpoint control.
roundtrip!(
    checkpoint_ack_roundtrips,
    CheckpointAck,
    support::checkpoint_ack()
);
roundtrip!(
    checkpoint_reason_roundtrips,
    CheckpointReason,
    support::checkpoint_reason()
);

// Forge credential bridge.
roundtrip!(
    forge_request_roundtrips,
    ForgeRequest,
    support::forge_request()
);
roundtrip!(
    forge_response_roundtrips,
    ForgeResponse,
    support::forge_response()
);

// Artifact upload bridge.
roundtrip!(
    upload_request_roundtrips,
    UploadRequest,
    support::upload_request()
);
roundtrip!(
    upload_response_roundtrips,
    UploadResponse,
    support::upload_response()
);

// Port relay handshake.
roundtrip!(
    relay_connect_roundtrips,
    RelayConnect,
    support::relay_connect()
);
roundtrip!(relay_ack_roundtrips, RelayAck, support::relay_ack());
