//! ADR 0099 H4: decode-never-panics property suite for the substrate
//! populate protocol (`engram-substrate-proto`).
//!
//! Threat tier: below the guest→host harness/agentd boundary but above the
//! version-fenced coord↔host wire. The populate socket is a host-local UDS
//! between the chunk-cache writer and its uffd-handler clients, but the
//! handler decodes on the page-fault path — a panic there wedges a vCPU, so
//! the never-panic property is load-bearing regardless of who can reach the
//! socket.
//!
//! Three shapes, per ADR 0099 §H4:
//!   (a) arbitrary byte blobs → every public frame decode never panics;
//!   (b) mutations of valid frames (truncate every prefix / flip / append)
//!       never panic;
//!   (c) allocation bound: `read_frame` enforces `MAX_MSG_BYTES` before the
//!       body `vec!`, and a huge internal length prefix errors rather than
//!       over-allocating.

use std::io::Cursor;

use engram_core::types::manifest::ManifestRef;
use engram_substrate_proto::{read_frame, FromWriter, ToWriter, MAX_MSG_BYTES};
use proptest::prelude::*;

// ---- helpers -----------------------------------------------------------

fn decode_every_type(bytes: &[u8]) {
    let _ = bincode::deserialize::<ToWriter>(bytes);
    let _ = bincode::deserialize::<FromWriter>(bytes);
    // The framed entry point (length prefix + bincode).
    let _ = read_frame::<_, ToWriter>(&mut Cursor::new(bytes));
    let _ = read_frame::<_, FromWriter>(&mut Cursor::new(bytes));
}

fn manifest_ref() -> impl Strategy<Value = ManifestRef> {
    // Fresh random manifest_id per case; vary the version. Fields are pub.
    any::<u64>().prop_map(|version| ManifestRef {
        manifest_id: ManifestRef::new().manifest_id,
        version,
    })
}

fn to_writer() -> impl Strategy<Value = ToWriter> {
    prop_oneof![
        (
            any::<u32>(),
            proptest::option::of(manifest_ref()),
            proptest::option::of(manifest_ref()),
        )
            .prop_map(|(proto_version, canonical_manifest, session_manifest)| {
                ToWriter::Hello {
                    proto_version,
                    canonical_manifest,
                    session_manifest,
                }
            }),
        any::<[u8; 32]>().prop_map(|hash| ToWriter::Populate { hash }),
    ]
}

fn from_writer() -> impl Strategy<Value = FromWriter> {
    prop_oneof![
        (any::<bool>(), any::<bool>()).prop_map(|(tmpfs_ok, cache_writable)| {
            FromWriter::HelloAck {
                tmpfs_ok,
                cache_writable,
            }
        }),
        any::<u64>().prop_map(|len| FromWriter::Populated { len }),
        "[ -~]{0,16}".prop_map(|msg| FromWriter::PopulateErr { msg }),
    ]
}

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
    }
}

// ---- shape (b): mutations of valid frames ------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mutated_to_writer_never_panic(
        msg in to_writer(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&msg).expect("encode");
        mutate_and_decode::<ToWriter>(&encoded, flip_pos, flip_val, &garbage);
    }

    #[test]
    fn mutated_from_writer_never_panic(
        msg in from_writer(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&msg).expect("encode");
        mutate_and_decode::<FromWriter>(&encoded, flip_pos, flip_val, &garbage);
    }
}

// ---- shape (c): allocation bound ---------------------------------------

#[test]
fn oversized_length_prefix_rejected_before_alloc() {
    // 64 KiB cap here; a header over it must be rejected before the body
    // `vec!`. Only the 4-byte header is supplied.
    let header = (MAX_MSG_BYTES as u32 + 1).to_be_bytes();
    let err = read_frame::<_, ToWriter>(&mut Cursor::new(header.to_vec()))
        .expect_err("oversized frame must be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn huge_internal_length_errors_without_alloc() {
    // FromWriter::PopulateErr { msg: String } — variant index then a u64
    // string length claiming ~18 EiB, with no bytes following.
    let idx = {
        let enc = bincode::serialize(&FromWriter::PopulateErr { msg: String::new() }).unwrap();
        u32::from_le_bytes([enc[0], enc[1], enc[2], enc[3]])
    };
    let mut body = Vec::new();
    body.extend_from_slice(&idx.to_le_bytes());
    body.extend_from_slice(&u64::MAX.to_le_bytes());
    let err = bincode::deserialize::<FromWriter>(&body)
        .expect_err("huge internal length must error, not OOM");
    let _ = err;
}
