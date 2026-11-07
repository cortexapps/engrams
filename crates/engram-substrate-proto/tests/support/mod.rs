//! Shared valid-value proptest strategies for the substrate populate protocol
//! (`engram-substrate-proto`), consumed by BOTH property suites:
//!   - `decode_never_panics.rs` (ADR 0099 §H4) — mutates these valid frames;
//!   - `codec_roundtrip.rs` (ADR 0099 §H3) — asserts encode→decode identity.
//!
//! Each consumer links a subset, so the module carries a narrow
//! `allow(dead_code)`. The `_exhaustiveness_*` guards below make a new enum
//! variant a COMPILE error until a generator arm is added. NO wildcard arms.
#![allow(dead_code)]

use engram_core::types::manifest::ManifestRef;
use engram_substrate_proto::{ByteRange, FromWriter, Requester, ToWriter};
use proptest::prelude::*;

pub fn manifest_ref() -> impl Strategy<Value = ManifestRef> {
    // Fresh random manifest_id per case; vary the version. Fields are pub.
    any::<u64>().prop_map(|version| ManifestRef {
        manifest_id: ManifestRef::new().manifest_id,
        version,
    })
}

pub fn to_writer() -> impl Strategy<Value = ToWriter> {
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
        (
            any::<[u8; 32]>(),
            requester(),
            proptest::option::of((any::<u64>(), any::<u64>())),
        )
            .prop_map(|(hash, requester, range)| ToWriter::Read {
                hash,
                requester,
                range: range.map(|(offset, len)| ByteRange { offset, len }),
            }),
    ]
}

pub fn requester() -> impl Strategy<Value = Requester> {
    prop_oneof![Just(Requester::Memory), Just(Requester::Disk)]
}

pub fn from_writer() -> impl Strategy<Value = FromWriter> {
    prop_oneof![
        (any::<bool>(), any::<bool>()).prop_map(|(tmpfs_ok, cache_writable)| {
            FromWriter::HelloAck {
                tmpfs_ok,
                cache_writable,
            }
        }),
        any::<u64>().prop_map(|len| FromWriter::Populated { len }),
        "[ -~]{0,16}".prop_map(|msg| FromWriter::PopulateErr { msg }),
        any::<u64>().prop_map(|len| FromWriter::Filled { len }),
    ]
}

// ---- exhaustiveness guards ---------------------------------------------
//
// Never called; compiled only so the `match` is checked. A new enum variant
// makes the match non-exhaustive → compile error → land here and add its arm
// AND a generator arm to the matching strategy above. NO wildcard arms.

fn _exhaustiveness_to_writer(m: &ToWriter) {
    match m {
        ToWriter::Hello { .. } => {}
        ToWriter::Populate { .. } => {}
        ToWriter::Read { .. } => {}
    }
}

fn _exhaustiveness_requester(r: &Requester) {
    match r {
        Requester::Memory => {}
        Requester::Disk => {}
    }
}

fn _exhaustiveness_from_writer(m: &FromWriter) {
    match m {
        FromWriter::HelloAck { .. } => {}
        FromWriter::Populated { .. } => {}
        FromWriter::PopulateErr { .. } => {}
        FromWriter::Filled { .. } => {}
    }
}
