//! ADR 0099 H4: decode-never-panics property suite for the harness ↔ host
//! vsock wire (`engram-harness-proto`).
//!
//! This is the **highest-priority** decode surface in the program: the
//! harness channel is guest→host, and once user code runs inside the
//! sandbox every byte on it is attacker-influenceable (a compromised or
//! buggy in-guest harness, or the agent itself, can send whatever it
//! likes). A decode that panics on a malformed frame is a host-side DoS.
//!
//! Three shapes, per ADR 0099 §H4:
//!   (a) arbitrary byte blobs → every public frame decode returns Ok/Err,
//!       never panics;
//!   (b) mutations of *valid* frames — truncate at every prefix, flip a
//!       byte, append garbage — never panic;
//!   (c) a length prefix claiming a huge size must not allocate before
//!       validation: the reader enforces `MAX_MSG_BYTES` before the body
//!       `vec!`, and bincode's slice decode is bounded by the frame it was
//!       handed (a huge *internal* length prefix errors, never OOMs).
//!
//! The wire is positional bincode (see `src/lib.rs`): the frame types are
//! all externally-tagged enums / plain structs on purpose — an
//! internally-tagged or untagged enum would call serde `deserialize_any`,
//! which bincode does not support (documented decode-panic hazard on
//! `FileChange`). These properties are the regression guard on that
//! invariant.

use std::io::Cursor;

use engram_harness_proto::{
    read_msg, AgentRole, AttachReject, CheckpointAck, CheckpointReason, EditHunk, FileChange,
    ForgeOp, ForgeRequest, ForgeResponse, HarnessAttach, HarnessAttachAck, HarnessCommand,
    HarnessEvent, HarnessFrame, RelayAck, RelayConnect, UploadOp, UploadRequest, UploadResponse,
    MAX_MSG_BYTES,
};
use proptest::prelude::*;

// ---- helpers -----------------------------------------------------------

/// Decode `bytes` into EVERY public wire type. Each `deserialize` result is
/// deliberately discarded — the property under test is only that none of
/// them *panic*. A panic here unwinds out of the closure and fails the
/// proptest case with the shrunk counterexample.
fn decode_every_type(bytes: &[u8]) {
    let _ = bincode::deserialize::<HarnessAttach>(bytes);
    let _ = bincode::deserialize::<HarnessAttachAck>(bytes);
    let _ = bincode::deserialize::<AttachReject>(bytes);
    let _ = bincode::deserialize::<HarnessFrame>(bytes);
    let _ = bincode::deserialize::<HarnessEvent>(bytes);
    let _ = bincode::deserialize::<HarnessCommand>(bytes);
    let _ = bincode::deserialize::<FileChange>(bytes);
    let _ = bincode::deserialize::<EditHunk>(bytes);
    let _ = bincode::deserialize::<AgentRole>(bytes);
    let _ = bincode::deserialize::<CheckpointReason>(bytes);
    let _ = bincode::deserialize::<CheckpointAck>(bytes);
    let _ = bincode::deserialize::<ForgeRequest>(bytes);
    let _ = bincode::deserialize::<ForgeResponse>(bytes);
    let _ = bincode::deserialize::<ForgeOp>(bytes);
    let _ = bincode::deserialize::<UploadRequest>(bytes);
    let _ = bincode::deserialize::<UploadResponse>(bytes);
    let _ = bincode::deserialize::<UploadOp>(bytes);
    let _ = bincode::deserialize::<RelayConnect>(bytes);
    let _ = bincode::deserialize::<RelayAck>(bytes);
}

/// Drive the *actual framing entry point* (`read_msg`) over arbitrary bytes
/// on a small current-thread runtime. Covers the length-prefix path, not
/// just the raw bincode body decode.
fn read_frame_every_type(bytes: &[u8]) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let _ = read_msg::<_, HarnessFrame>(&mut Cursor::new(bytes)).await;
        let _ = read_msg::<_, HarnessAttach>(&mut Cursor::new(bytes)).await;
        let _ = read_msg::<_, ForgeRequest>(&mut Cursor::new(bytes)).await;
        let _ = read_msg::<_, UploadRequest>(&mut Cursor::new(bytes)).await;
        let _ = read_msg::<_, RelayConnect>(&mut Cursor::new(bytes)).await;
    });
}

// ---- strategies for VALID frames ---------------------------------------
//
// The valid-value strategies live in the shared `support` module (kept small
// so the truncation sweep in shape (b) stays cheap); ADR 0099 H3's
// `codec_roundtrip.rs` reuses the exact same generators, so a new frame shape
// is exercised by both suites from one definition.
mod support;
use support::{forge_request, harness_attach, harness_frame, upload_request};

/// Apply the three mutation classes to `encoded` and decode `T` from each
/// result, asserting none panic. `truncate at every prefix` is a full
/// sweep — cheap because the strategies keep frames to a few hundred bytes.
fn mutate_and_decode<T: serde::de::DeserializeOwned>(
    encoded: &[u8],
    flip_pos: usize,
    flip_val: u8,
    garbage: &[u8],
) {
    // (i) truncate at EVERY prefix length (0..=len).
    for cut in 0..=encoded.len() {
        let _ = bincode::deserialize::<T>(&encoded[..cut]);
    }
    // (ii) flip one byte.
    if !encoded.is_empty() {
        let mut m = encoded.to_vec();
        let p = flip_pos % m.len();
        m[p] ^= flip_val.max(1);
        let _ = bincode::deserialize::<T>(&m);
    }
    // (iii) append garbage.
    let mut ext = encoded.to_vec();
    ext.extend_from_slice(garbage);
    let _ = bincode::deserialize::<T>(&ext);
}

// ---- shape (a): arbitrary bytes ---------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Arbitrary byte blobs (weighted small, up to 64 KiB) decode into every
    /// public wire type without panicking.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in weighted_blob()) {
        decode_every_type(&bytes);
        read_frame_every_type(&bytes);
    }
}

/// Byte blob weighted toward small sizes (the common adversarial shape),
/// with a thin tail up to 64 KiB.
fn weighted_blob() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        10 => proptest::collection::vec(any::<u8>(), 0..=256),
        3 => proptest::collection::vec(any::<u8>(), 257..=4096),
        1 => proptest::collection::vec(any::<u8>(), 4097..=65536),
    ]
}

// ---- shape (b): mutations of valid frames ------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mutated_harness_frames_never_panic(
        frame in harness_frame(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&frame).expect("encode");
        mutate_and_decode::<HarnessFrame>(&encoded, flip_pos, flip_val, &garbage);
    }

    #[test]
    fn mutated_handshake_frames_never_panic(
        attach in harness_attach(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&attach).expect("encode");
        mutate_and_decode::<HarnessAttach>(&encoded, flip_pos, flip_val, &garbage);
    }

    #[test]
    fn mutated_bridge_frames_never_panic(
        forge in forge_request(),
        upload in upload_request(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let f = bincode::serialize(&forge).expect("encode");
        mutate_and_decode::<ForgeRequest>(&f, flip_pos, flip_val, &garbage);
        let u = bincode::serialize(&upload).expect("encode");
        mutate_and_decode::<UploadRequest>(&u, flip_pos, flip_val, &garbage);
    }
}

// ---- shape (c): allocation bound ---------------------------------------

/// A length prefix over `MAX_MSG_BYTES` is rejected by `read_msg` BEFORE it
/// allocates the body buffer — a 4-byte header alone triggers the error.
#[test]
fn oversized_length_prefix_rejected_before_alloc() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        // Only the 4-byte header, no body: if the reader tried to allocate
        // `len` bytes this would OOM; instead it must return InvalidData.
        let header = (MAX_MSG_BYTES as u32 + 1).to_be_bytes();
        let err = read_msg::<_, HarnessFrame>(&mut Cursor::new(header.to_vec()))
            .await
            .expect_err("oversized frame must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("MAX_MSG_BYTES"));
    });
}

/// A frame body whose *internal* length prefix (e.g. a `String`/`Vec`
/// length) claims a huge size must error, not attempt a huge allocation.
/// bincode's slice reader bounds every read by the buffer it was handed, so
/// the decode fails fast on unexpected end rather than pre-allocating.
#[test]
fn huge_internal_length_errors_without_alloc() {
    // HarnessEvent::TitleSuggested { title: String } — variant index then a
    // u64 string length claiming ~18 EiB, with no bytes following.
    let title_idx = {
        // Discover the variant index by encoding a real instance.
        let enc = bincode::serialize(&HarnessEvent::TitleSuggested {
            title: String::new(),
        })
        .unwrap();
        u32::from_le_bytes([enc[0], enc[1], enc[2], enc[3]])
    };
    let mut body = Vec::new();
    body.extend_from_slice(&title_idx.to_le_bytes());
    body.extend_from_slice(&u64::MAX.to_le_bytes());
    let err = bincode::deserialize::<HarnessEvent>(&body)
        .expect_err("huge internal length must error, not OOM");
    // The exact error is bincode's "unexpected end"; asserting it returns at
    // all (no panic/abort) is the property.
    let _ = err;
}
