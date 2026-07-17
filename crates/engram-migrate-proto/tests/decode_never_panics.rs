//! ADR 0099 H4: decode-never-panics property suite for the ADR 0045 C2
//! post-copy page channel (`engram-migrate-proto`).
//!
//! Threat tier: host↔host over TCP with per-export token auth (and a
//! host-local control UDS). Below the guest→host boundary, but the dest
//! uffd-handler decodes these frames on the page-fault path, so a decode
//! panic wedges a restoring guest. This suite also fuzzes the two
//! decode-adjacent surfaces on the same untrusted bytes: `decompress_page`
//! (lz4) and `SealBitmap`'s accessors/`validate`.
//!
//! Three shapes, per ADR 0099 §H4:
//!   (a) arbitrary byte blobs → every public frame decode never panics;
//!   (b) mutations of valid frames (truncate every prefix / flip / append)
//!       never panic;
//!   (c) allocation bound: `read_frame` enforces `MAX_FRAME_BYTES` before
//!       the body `vec!`, and a huge internal length prefix errors rather
//!       than over-allocating.

use std::io::Cursor;

use engram_migrate_proto::{
    decompress_page, read_frame, ConnPurpose, FromSource, HandlerControl, SealBitmap, ToSource,
    MAX_FRAME_BYTES,
};
use proptest::prelude::*;

// The valid-value strategies (small values keep the shape-(b) prefix sweep
// cheap) live in the shared `support` module; ADR 0099 H3's
// `codec_roundtrip.rs` reuses the exact same generators.
mod support;
use support::{from_source, handler_control, seal_bitmap, to_source};

// ---- helpers -----------------------------------------------------------

fn decode_every_type(bytes: &[u8]) {
    let _ = bincode::deserialize::<ToSource>(bytes);
    let _ = bincode::deserialize::<FromSource>(bytes);
    let _ = bincode::deserialize::<HandlerControl>(bytes);
    let _ = bincode::deserialize::<SealBitmap>(bytes);
    let _ = bincode::deserialize::<ConnPurpose>(bytes);
    let _ = read_frame::<_, ToSource>(&mut Cursor::new(bytes));
    let _ = read_frame::<_, FromSource>(&mut Cursor::new(bytes));
    let _ = read_frame::<_, HandlerControl>(&mut Cursor::new(bytes));
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

    /// `decompress_page` never panics. In production it runs only on bytes
    /// whose blake3 wire hash already matched (see the fn's doc), so we
    /// exercise the real domain: valid `compress_page` output plus small
    /// corruptions/truncations of it. Low-entropy input keeps the frame a
    /// genuine lz4 block, so its size prefix stays the small real length —
    /// corrupting the BODY (never the prefix) can't drive a speculative
    /// giant allocation, which is a property of `lz4_flex` upstream of our
    /// never-panic guarantee, not something a hash-gated caller ever hits.
    #[test]
    fn decompress_page_never_panics(
        // Bytes over a tiny alphabet compress well ⇒ lz4 = true reliably.
        raw in proptest::collection::vec(0u8..=3u8, 8..=4096),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        trunc in any::<prop::sample::Index>(),
    ) {
        // Raw passthrough (lz4 = false) is the identity — always Ok.
        prop_assert!(decompress_page(raw.clone(), false).is_ok());

        let (wire, lz4) = engram_migrate_proto::compress_page(raw.clone());
        prop_assume!(lz4); // only meaningful for genuine lz4 frames
        // A faithful round-trip recovers the original.
        prop_assert_eq!(decompress_page(wire.clone(), lz4).unwrap(), raw);

        // Corrupt the BODY (never the 4-byte size prefix): flip a byte, then
        // decode — Err or a differing payload, never a panic.
        if wire.len() > 4 {
            let mut m = wire.clone();
            let p = 4 + (flip_pos % (m.len() - 4));
            m[p] ^= flip_val.max(1);
            let _ = decompress_page(m, true);
        }
        // Truncate the BODY (keep the intact prefix) and decode — Err, not
        // panic.
        let cut = 4 + trunc.index(wire.len() - 3);
        let _ = decompress_page(wire[..cut.min(wire.len())].to_vec(), true);
    }

    /// `SealBitmap` accessors and `validate` never panic on an arbitrary
    /// (possibly structurally-invalid) bitmap — the handler runs `validate`
    /// and `get` on the fault path.
    #[test]
    fn seal_bitmap_accessors_never_panic(
        bitmap in seal_bitmap(),
        probe_idx in any::<u64>(),
    ) {
        let _ = bitmap.validate();
        let _ = bitmap.get(probe_idx);
        let _ = bitmap.get(u64::MAX);
        let _ = bitmap.count_ones();
    }
}

// ---- shape (b): mutations of valid frames ------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mutated_to_source_never_panic(
        msg in to_source(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&msg).expect("encode");
        mutate_and_decode::<ToSource>(&encoded, flip_pos, flip_val, &garbage);
    }

    #[test]
    fn mutated_from_source_never_panic(
        msg in from_source(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&msg).expect("encode");
        mutate_and_decode::<FromSource>(&encoded, flip_pos, flip_val, &garbage);
    }

    #[test]
    fn mutated_handler_control_never_panic(
        msg in handler_control(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&msg).expect("encode");
        mutate_and_decode::<HandlerControl>(&encoded, flip_pos, flip_val, &garbage);
    }
}

// ---- shape (c): allocation bound ---------------------------------------

#[test]
fn oversized_length_prefix_rejected_before_alloc() {
    let header = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
    let err = read_frame::<_, FromSource>(&mut Cursor::new(header.to_vec()))
        .expect_err("oversized frame must be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("MAX_FRAME_BYTES"));
}

#[test]
fn huge_internal_length_errors_without_alloc() {
    // FromSource::Error { req_id: Option<u64>, message: String }. Encode a
    // real value to discover the variant index, then craft: idx, req_id
    // None (1 byte tag 0), then a u64 String length claiming ~18 EiB.
    let idx = {
        let enc = bincode::serialize(&FromSource::Error {
            req_id: None,
            message: String::new(),
        })
        .unwrap();
        u32::from_le_bytes([enc[0], enc[1], enc[2], enc[3]])
    };
    let mut body = Vec::new();
    body.extend_from_slice(&idx.to_le_bytes());
    body.push(0u8); // Option<u64>::None
    body.extend_from_slice(&u64::MAX.to_le_bytes());
    let err = bincode::deserialize::<FromSource>(&body)
        .expect_err("huge internal length must error, not OOM");
    let _ = err;
}
