//! ADR 0099 H4: decode-never-panics property suite for the host ↔ in-guest
//! agentd vsock wire (`engram_agentd::proto`).
//!
//! Second-highest priority after `engram-harness-proto`: agentd runs inside
//! the sandbox, so the frames the host reads back from it
//! (`WireResponse`, `WireExecEvent`, `AgentReady`) are attacker-
//! influenceable once user code runs in the guest. A host-side decode that
//! panics on a malformed reply is a DoS on the host-agent.
//!
//! Three shapes, per ADR 0099 §H4 — see the harness-proto suite header for
//! the full rationale:
//!   (a) arbitrary byte blobs → every public frame decode never panics;
//!   (b) mutations of valid frames (truncate every prefix / flip / append)
//!       never panic;
//!   (c) allocation bound: `MAX_MSG_BYTES` enforced before the body `vec!`,
//!       and a huge internal length prefix errors rather than over-allocating.

use std::io::Cursor;

use engram_agentd::proto::{
    read_msg, AgentReady, SpawnHarnessRequest, WireDownloadResponse, WireExecEvent,
    WireExecRequest, WireFileChunk, WireHandshake, WireHandshakeAck, WireRequest, WireResponse,
    WireStatResponse, MAX_MSG_BYTES,
};
use proptest::prelude::*;

// ---- helpers -----------------------------------------------------------

fn decode_every_type(bytes: &[u8]) {
    let _ = bincode::deserialize::<WireRequest>(bytes);
    let _ = bincode::deserialize::<WireResponse>(bytes);
    let _ = bincode::deserialize::<WireExecEvent>(bytes);
    let _ = bincode::deserialize::<WireExecRequest>(bytes);
    let _ = bincode::deserialize::<SpawnHarnessRequest>(bytes);
    let _ = bincode::deserialize::<WireHandshake>(bytes);
    let _ = bincode::deserialize::<WireHandshakeAck>(bytes);
    let _ = bincode::deserialize::<AgentReady>(bytes);
    let _ = bincode::deserialize::<WireStatResponse>(bytes);
    let _ = bincode::deserialize::<WireDownloadResponse>(bytes);
    let _ = bincode::deserialize::<WireFileChunk>(bytes);
}

fn read_frame_every_type(bytes: &[u8]) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let _ = read_msg::<_, WireRequest>(&mut Cursor::new(bytes)).await;
        let _ = read_msg::<_, WireResponse>(&mut Cursor::new(bytes)).await;
        let _ = read_msg::<_, WireExecEvent>(&mut Cursor::new(bytes)).await;
        let _ = read_msg::<_, AgentReady>(&mut Cursor::new(bytes)).await;
        let _ = read_msg::<_, WireFileChunk>(&mut Cursor::new(bytes)).await;
    });
}

// ---- strategies --------------------------------------------------------
//
// The valid-value strategies live in the shared `support` module (small
// values keep the shape-(b) prefix sweep cheap); ADR 0099 H3's
// `codec_roundtrip.rs` reuses the exact same generators.
mod support;
use support::{wire_exec_event, wire_request, wire_response};

fn mutate_and_decode<T: serde::de::DeserializeOwned>(
    encoded: &[u8],
    flip_pos: usize,
    flip_val: u8,
    garbage: &[u8],
) {
    for cut in 0..=encoded.len() {
        let _ = bincode::deserialize::<T>(&encoded[..cut]);
    }
    if !encoded.is_empty() {
        let mut m = encoded.to_vec();
        let p = flip_pos % m.len();
        m[p] ^= flip_val.max(1);
        let _ = bincode::deserialize::<T>(&m);
    }
    let mut ext = encoded.to_vec();
    ext.extend_from_slice(garbage);
    let _ = bincode::deserialize::<T>(&ext);
}

fn weighted_blob() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        10 => proptest::collection::vec(any::<u8>(), 0..=256),
        3 => proptest::collection::vec(any::<u8>(), 257..=4096),
        1 => proptest::collection::vec(any::<u8>(), 4097..=65536),
    ]
}

// ---- shape (a): arbitrary bytes ---------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn arbitrary_bytes_never_panic(bytes in weighted_blob()) {
        decode_every_type(&bytes);
        read_frame_every_type(&bytes);
    }
}

// ---- shape (b): mutations of valid frames ------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mutated_wire_requests_never_panic(
        req in wire_request(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&req).expect("encode");
        mutate_and_decode::<WireRequest>(&encoded, flip_pos, flip_val, &garbage);
    }

    #[test]
    fn mutated_wire_responses_never_panic(
        resp in wire_response(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&resp).expect("encode");
        mutate_and_decode::<WireResponse>(&encoded, flip_pos, flip_val, &garbage);
    }

    #[test]
    fn mutated_exec_events_never_panic(
        ev in wire_exec_event(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&ev).expect("encode");
        mutate_and_decode::<WireExecEvent>(&encoded, flip_pos, flip_val, &garbage);
    }
}

// ---- shape (c): allocation bound ---------------------------------------

#[test]
fn oversized_length_prefix_rejected_before_alloc() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let header = (MAX_MSG_BYTES as u32 + 1).to_be_bytes();
        let err = read_msg::<_, WireResponse>(&mut Cursor::new(header.to_vec()))
            .await
            .expect_err("oversized frame must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("MAX_MSG_BYTES"));
    });
}

#[test]
fn huge_internal_length_errors_without_alloc() {
    // WireExecEvent::Stdout(Vec<u8>) — variant index 0, then a u64 Vec
    // length claiming ~18 EiB, with no bytes following. The slice decoder
    // must error on unexpected end, never pre-allocate the claimed length.
    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&u64::MAX.to_le_bytes());
    let err = bincode::deserialize::<WireExecEvent>(&body)
        .expect_err("huge internal length must error, not OOM");
    let _ = err;
}
