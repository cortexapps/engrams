//! ADR 0099 H3 (PR 2): codec ROUND-TRIP identity for the substrate populate
//! protocol (`engram-substrate-proto`).
//!
//! Complement to `decode_never_panics.rs` (H4): those prove garbage never
//! panics on the page-fault decode path; these prove `encode → decode ==
//! identity` for arbitrary valid `ToWriter`/`FromWriter` frames, catching an
//! encode/decode asymmetry on a new field a single golden value would miss.
//!
//! Valid-value strategies are shared with the decode suite via
//! `tests/support/mod.rs`.

use engram_substrate_proto::{FromWriter, ToWriter};
use proptest::prelude::*;

mod support;

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

roundtrip!(to_writer_roundtrips, ToWriter, support::to_writer());
roundtrip!(from_writer_roundtrips, FromWriter, support::from_writer());
