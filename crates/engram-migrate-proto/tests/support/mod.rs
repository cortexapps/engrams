//! Shared valid-value proptest strategies for the ADR 0045 C2 post-copy page
//! channel (`engram-migrate-proto`), consumed by BOTH property suites:
//!   - `decode_never_panics.rs` (ADR 0099 §H4) — mutates these valid frames;
//!   - `codec_roundtrip.rs` (ADR 0099 §H3) — asserts encode→decode identity.
//!
//! Each consumer links a subset, so the module carries a narrow
//! `allow(dead_code)`. The `_exhaustiveness_*` guards below make a new enum
//! variant a COMPILE error until a generator arm is added. NO wildcard arms.
#![allow(dead_code)]

use engram_migrate_proto::{ConnPurpose, FromSource, HandlerControl, SealBitmap, ToSource};
use proptest::prelude::*;

pub fn s() -> impl Strategy<Value = String> {
    "[ -~]{0,10}"
}

pub fn small_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..=32)
}

pub fn conn_purpose() -> impl Strategy<Value = ConnPurpose> {
    prop_oneof![Just(ConnPurpose::Fault), Just(ConnPurpose::Drain)]
}

/// Arbitrary (not necessarily `validate`-passing) SealBitmap — all fields
/// are pub, so any combination is a decodable value. `validate` is expected
/// to *reject* the invalid ones; the property is that it never panics.
pub fn seal_bitmap() -> impl Strategy<Value = SealBitmap> {
    (any::<u64>(), any::<u64>(), small_bytes()).prop_map(|(chunk_size, chunk_count, bits)| {
        SealBitmap {
            chunk_size,
            chunk_count,
            bits,
        }
    })
}

pub fn to_source() -> impl Strategy<Value = ToSource> {
    prop_oneof![
        (any::<u32>(), s(), s(), conn_purpose()).prop_map(
            |(version, export_id, token, purpose)| ToSource::Hello {
                version,
                export_id,
                token,
                purpose,
            }
        ),
        (any::<u64>(), any::<u64>()).prop_map(|(req_id, chunk_offset)| ToSource::NeedAt {
            req_id,
            chunk_offset
        }),
        (any::<u64>(), any::<[u8; 32]>())
            .prop_map(|(req_id, hash)| ToSource::GetChunk { req_id, hash }),
        (any::<u64>(), any::<u64>(), any::<u64>()).prop_map(
            |(pulled, alt_sourced, zero_chunks)| ToSource::DrainDone {
                pulled,
                alt_sourced,
                zero_chunks,
            }
        ),
    ]
}

pub fn from_source() -> impl Strategy<Value = FromSource> {
    prop_oneof![
        (any::<u32>(), any::<u64>(), any::<u64>()).prop_map(
            |(version, chunk_size, total_bytes)| {
                FromSource::HelloAck {
                    version,
                    chunk_size,
                    total_bytes,
                }
            }
        ),
        seal_bitmap().prop_map(|bitmap| FromSource::Seal { bitmap }),
        (
            any::<u64>(),
            any::<u64>(),
            small_bytes(),
            any::<[u8; 32]>(),
            any::<bool>()
        )
            .prop_map(
                |(req_id, chunk_offset, bytes, hash, lz4)| FromSource::Page {
                    req_id,
                    chunk_offset,
                    bytes,
                    hash,
                    lz4,
                }
            ),
        (any::<u64>(), any::<u64>()).prop_map(|(req_id, chunk_offset)| FromSource::ZeroChunk {
            req_id,
            chunk_offset
        }),
        (any::<u64>(), any::<u64>(), any::<[u8; 32]>()).prop_map(
            |(req_id, chunk_offset, durable_sha256)| FromSource::AltSource {
                req_id,
                chunk_offset,
                durable_sha256,
            }
        ),
        (any::<u64>(), small_bytes())
            .prop_map(|(req_id, bytes)| FromSource::ChunkBytes { req_id, bytes }),
        (proptest::option::of(any::<u64>()), s())
            .prop_map(|(req_id, message)| FromSource::Error { req_id, message }),
    ]
}

pub fn handler_control() -> impl Strategy<Value = HandlerControl> {
    prop_oneof![
        (any::<u64>(), any::<u64>(), any::<i64>()).prop_map(
            |(dirty_chunks, total_chunks, at_unix_ms)| {
                HandlerControl::Sealed {
                    dirty_chunks,
                    total_chunks,
                    at_unix_ms,
                }
            }
        ),
        (any::<u64>(), any::<u64>())
            .prop_map(|(pulled, remaining)| HandlerControl::DrainProgress { pulled, remaining }),
        (
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
        )
            .prop_map(
                |(pulled, alt_sourced, zero_chunks, ms, faults, fault_us, fault_max_us)| {
                    HandlerControl::DrainDone {
                        pulled,
                        alt_sourced,
                        zero_chunks,
                        ms,
                        faults,
                        fault_us,
                        fault_max_us,
                    }
                }
            ),
        (any::<u64>(), s())
            .prop_map(|(remaining, detail)| HandlerControl::PeerLost { remaining, detail }),
    ]
}

// ---- exhaustiveness guards ---------------------------------------------
//
// Never called; compiled only so the `match` is checked. A new enum variant
// makes the match non-exhaustive → compile error → land here and add its arm
// AND a generator arm to the matching strategy above. NO wildcard arms.

fn _exhaustiveness_conn_purpose(p: &ConnPurpose) {
    match p {
        ConnPurpose::Fault => {}
        ConnPurpose::Drain => {}
    }
}

fn _exhaustiveness_to_source(m: &ToSource) {
    match m {
        ToSource::Hello { .. } => {}
        ToSource::NeedAt { .. } => {}
        ToSource::GetChunk { .. } => {}
        ToSource::DrainDone { .. } => {}
    }
}

fn _exhaustiveness_from_source(m: &FromSource) {
    match m {
        FromSource::HelloAck { .. } => {}
        FromSource::Seal { .. } => {}
        FromSource::Page { .. } => {}
        FromSource::ZeroChunk { .. } => {}
        FromSource::AltSource { .. } => {}
        FromSource::ChunkBytes { .. } => {}
        FromSource::Error { .. } => {}
    }
}

fn _exhaustiveness_handler_control(m: &HandlerControl) {
    match m {
        HandlerControl::Sealed { .. } => {}
        HandlerControl::DrainProgress { .. } => {}
        HandlerControl::DrainDone { .. } => {}
        HandlerControl::PeerLost { .. } => {}
    }
}
